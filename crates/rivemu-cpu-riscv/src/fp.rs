//! F and D extension execution.

use rivemu_core::AccessSize;
use rivemu_mem::AddressSpace;
use rivemu_softfloat::{self as sf, F32_CANONICAL_NAN, Flags, RoundingMode};

use crate::csr::status;
use crate::decode::{Insn, Op};
use crate::hart::{Exception, Hart, cause};

const BOX: u64 = 0xffff_ffff_0000_0000;
const SIGN32: u32 = 1 << 31;
const SIGN64: u64 = 1 << 63;

impl Hart {
    /// Reads an f register as a single, unboxing (improperly boxed → canonical NaN).
    #[inline]
    fn f32(&self, r: u8) -> u32 {
        let v = self.f[r as usize];
        if v & BOX == BOX {
            v as u32
        } else {
            F32_CANONICAL_NAN
        }
    }

    #[inline]
    fn set_f32(&mut self, r: u8, v: u32) {
        self.f[r as usize] = BOX | u64::from(v);
        self.csr.mstatus |= status::FS;
    }

    #[inline]
    fn set_f64(&mut self, r: u8, v: u64) {
        self.f[r as usize] = v;
        self.csr.mstatus |= status::FS;
    }

    #[inline]
    fn accrue(&mut self, fl: Flags) {
        if fl.0 != 0 {
            self.csr.fflags |= u64::from(fl.0);
            self.csr.mstatus |= status::FS;
        }
    }

    pub(crate) fn exec_fp(&mut self, bus: &AddressSpace, insn: &Insn) -> Result<(), Exception> {
        let illegal = Exception::new(cause::ILLEGAL_INSN, u64::from(insn.raw));
        if self.csr.mstatus & status::FS == 0 {
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
        );
        let rm = match RoundingMode::from_bits(rm_bits) {
            Some(rm) => rm,
            None if uses_rm => return Err(illegal),
            None => RoundingMode::NearestEven,
        };
        let mut fl = Flags::default();
        let (rd, rs1, rs2, rs3) = (insn.rd, insn.rs1, insn.rs2, insn.rs3);
        let xs1 = self.x[rs1 as usize];

        match insn.op {
            Op::Fload => {
                let addr = xs1.wrapping_add(insn.imm as u64);
                if insn.dbl {
                    let v = self.load(bus, addr, AccessSize::Double)?;
                    self.set_f64(rd, v);
                } else {
                    let v = self.load(bus, addr, AccessSize::Word)?;
                    self.set_f32(rd, v as u32);
                }
            }
            Op::Fstore => {
                let addr = xs1.wrapping_add(insn.imm as u64);
                let size = if insn.dbl {
                    AccessSize::Double
                } else {
                    AccessSize::Word
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
                if insn.dbl {
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
                } else {
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
            Op::Fadd | Op::Fsub | Op::Fmul | Op::Fdiv => {
                if insn.dbl {
                    let (a, b) = (self.f[rs1 as usize], self.f[rs2 as usize]);
                    let f = match insn.op {
                        Op::Fadd => sf::f64_add,
                        Op::Fsub => sf::f64_sub,
                        Op::Fmul => sf::f64_mul,
                        _ => sf::f64_div,
                    };
                    let v = f(a, b, rm, &mut fl);
                    self.set_f64(rd, v);
                } else {
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
            }
            Op::Fsqrt => {
                if insn.dbl {
                    let v = sf::f64_sqrt(self.f[rs1 as usize], rm, &mut fl);
                    self.set_f64(rd, v);
                } else {
                    let v = sf::f32_sqrt(self.f32(rs1), rm, &mut fl);
                    self.set_f32(rd, v);
                }
            }
            Op::Fsgnj | Op::Fsgnjn | Op::Fsgnjx => {
                if insn.dbl {
                    let (a, b) = (self.f[rs1 as usize], self.f[rs2 as usize]);
                    let s = match insn.op {
                        Op::Fsgnj => b,
                        Op::Fsgnjn => !b,
                        _ => a ^ b,
                    } & SIGN64;
                    self.set_f64(rd, (a & !SIGN64) | s);
                } else {
                    let (a, b) = (self.f32(rs1), self.f32(rs2));
                    let s = match insn.op {
                        Op::Fsgnj => b,
                        Op::Fsgnjn => !b,
                        _ => a ^ b,
                    } & SIGN32;
                    self.set_f32(rd, (a & !SIGN32) | s);
                }
            }
            Op::Fmin | Op::Fmax => {
                let max = insn.op == Op::Fmax;
                if insn.dbl {
                    let (a, b) = (self.f[rs1 as usize], self.f[rs2 as usize]);
                    let v = if max {
                        sf::f64_max(a, b, &mut fl)
                    } else {
                        sf::f64_min(a, b, &mut fl)
                    };
                    self.set_f64(rd, v);
                } else {
                    let (a, b) = (self.f32(rs1), self.f32(rs2));
                    let v = if max {
                        sf::f32_max(a, b, &mut fl)
                    } else {
                        sf::f32_min(a, b, &mut fl)
                    };
                    self.set_f32(rd, v);
                }
            }
            Op::FcvtIntF => {
                let v = if insn.dbl {
                    let a = self.f[rs1 as usize];
                    match rs2 {
                        0 => sf::f64_to_i32(a, rm, &mut fl) as i64 as u64,
                        1 => sf::f64_to_u32(a, rm, &mut fl) as i32 as i64 as u64,
                        2 => sf::f64_to_i64(a, rm, &mut fl) as u64,
                        _ => sf::f64_to_u64(a, rm, &mut fl),
                    }
                } else {
                    let a = self.f32(rs1);
                    match rs2 {
                        0 => sf::f32_to_i32(a, rm, &mut fl) as i64 as u64,
                        1 => sf::f32_to_u32(a, rm, &mut fl) as i32 as i64 as u64,
                        2 => sf::f32_to_i64(a, rm, &mut fl) as u64,
                        _ => sf::f32_to_u64(a, rm, &mut fl),
                    }
                };
                self.set_x(rd, v);
            }
            Op::FcvtFInt => {
                if insn.dbl {
                    let v = match rs2 {
                        0 => sf::i32_to_f64(xs1 as i32, rm, &mut fl),
                        1 => sf::u32_to_f64(xs1 as u32, rm, &mut fl),
                        2 => sf::i64_to_f64(xs1 as i64, rm, &mut fl),
                        _ => sf::u64_to_f64(xs1, rm, &mut fl),
                    };
                    self.set_f64(rd, v);
                } else {
                    let v = match rs2 {
                        0 => sf::i32_to_f32(xs1 as i32, rm, &mut fl),
                        1 => sf::u32_to_f32(xs1 as u32, rm, &mut fl),
                        2 => sf::i64_to_f32(xs1 as i64, rm, &mut fl),
                        _ => sf::u64_to_f32(xs1, rm, &mut fl),
                    };
                    self.set_f32(rd, v);
                }
            }
            Op::FcvtFF => {
                if insn.dbl {
                    let v = sf::f32_to_f64(self.f32(rs1), &mut fl);
                    self.set_f64(rd, v);
                } else {
                    let v = sf::f64_to_f32(self.f[rs1 as usize], rm, &mut fl);
                    self.set_f32(rd, v);
                }
            }
            Op::FmvXF => {
                let v = self.f[rs1 as usize];
                let v = if insn.dbl { v } else { v as i32 as i64 as u64 };
                self.set_x(rd, v);
            }
            Op::FmvFX => {
                if insn.dbl {
                    self.set_f64(rd, xs1);
                } else {
                    self.set_f32(rd, xs1 as u32);
                }
            }
            Op::Feq | Op::Flt | Op::Fle => {
                let r = if insn.dbl {
                    let (a, b) = (self.f[rs1 as usize], self.f[rs2 as usize]);
                    match insn.op {
                        Op::Feq => sf::f64_eq(a, b, &mut fl),
                        Op::Flt => sf::f64_lt(a, b, &mut fl),
                        _ => sf::f64_le(a, b, &mut fl),
                    }
                } else {
                    let (a, b) = (self.f32(rs1), self.f32(rs2));
                    match insn.op {
                        Op::Feq => sf::f32_eq(a, b, &mut fl),
                        Op::Flt => sf::f32_lt(a, b, &mut fl),
                        _ => sf::f32_le(a, b, &mut fl),
                    }
                };
                self.set_x(rd, u64::from(r));
            }
            Op::Fclass => {
                let v = if insn.dbl {
                    sf::f64_classify(self.f[rs1 as usize])
                } else {
                    sf::f32_classify(self.f32(rs1))
                };
                self.set_x(rd, v);
            }
            _ => return Err(illegal),
        }
        self.accrue(fl);
        Ok(())
    }
}
