//! RISC-V Vector Extension (`RVV 1.0` / `V`, `VLEN = 256`, `ELEN = 64`) execution engine.

use rivemu_core::AccessSize;
use rivemu_mem::AddressSpace;
use rivemu_softfloat::{
    self as sf, F16_CANONICAL_NAN, F32_CANONICAL_NAN, F64_CANONICAL_NAN, Flags, RoundingMode,
};

use crate::csr::status;
use crate::decode::{Insn, Op};
use crate::hart::{Exception, Hart, cause};

#[inline]
fn mask_sew(val: u64, sew_bytes: usize) -> u64 {
    match sew_bytes {
        1 => val & 0xff,
        2 => val & 0xffff,
        4 => val & 0xffff_ffff,
        _ => val,
    }
}

#[inline]
fn sext_sew(val: u64, sew_bytes: usize) -> i64 {
    match sew_bytes {
        1 => val as u8 as i8 as i64,
        2 => val as u16 as i16 as i64,
        4 => val as u32 as i32 as i64,
        _ => val as i64,
    }
}

#[inline]
/// log2(LMUL) from `vtype.vlmul`, or `None` for the reserved encoding 4.
const fn lmul_pow(vtype: u64) -> Option<i32> {
    match vtype & 7 {
        0 => Some(0),
        1 => Some(1),
        2 => Some(2),
        3 => Some(3),
        5 => Some(-3),
        6 => Some(-2),
        7 => Some(-1),
        _ => None,
    }
}

/// log2 of an element width in bytes (the EEW/SEW ratio is the same in bits).
const fn width_pow(bytes: usize) -> i32 {
    bytes.trailing_zeros() as i32
}

/// Registers in a group of log2 multiplier `pow`; fractional groups use one.
const fn group_regs(pow: i32) -> usize {
    if pow <= 0 { 1 } else { 1 << pow }
}

/// `reg` is a valid register-group base for log2 multiplier `pow`.
const fn valid_reg_group(reg: usize, pow: i32) -> bool {
    reg % group_regs(pow) == 0
}

/// Segment accesses: `nf * EMUL <= 8` and the last field ends at or below v31.
const fn valid_segment(reg: usize, nf: usize, pow: i32) -> bool {
    let size_ok = if pow < 0 {
        nf / (1 << -pow) <= 8
    } else {
        (nf << pow) <= 8
    };
    size_ok && reg + nf * group_regs(pow) <= 32
}

/// RVV 1.0 section 5.2 overlap rule between a source group `rs` and a
/// destination group `rd` with different EMULs (Sail `valid_reg_overlap`).
const fn valid_reg_overlap(rs: usize, rd: usize, pow_rs: i32, pow_rd: i32) -> bool {
    let rs_group = group_regs(pow_rs);
    let rd_group = group_regs(pow_rd);
    if rs % rs_group != 0 || rd % rd_group != 0 {
        return false;
    }
    if pow_rs < pow_rd {
        rs + rs_group <= rd
            || rs >= rd + rd_group
            || (rs + rs_group == rd + rd_group && pow_rs >= 0)
    } else if pow_rs > pow_rd {
        rd <= rs || rd >= rs + rs_group
    } else {
        true
    }
}

/// `rs` does not overlap any of the `nf` fields starting at `rd`
/// (Sail `valid_disjoint_reg_groups_segmented`).
const fn valid_disjoint_segmented(
    rs: usize,
    rd: usize,
    pow_rs: i32,
    pow_rd: i32,
    nf: usize,
) -> bool {
    let rs_group = group_regs(pow_rs);
    let rd_group = group_regs(pow_rd);
    if rs % rs_group != 0 || rd % rd_group != 0 {
        return false;
    }
    rs + rs_group <= rd || rd + rd_group * nf <= rs
}

/// Source groups of different EEW may not overlap (Sail
/// `valid_src_overlap_segmented`).
const fn valid_src_overlap_segmented(
    rs1: usize,
    eew1: usize,
    pow1: i32,
    rs2: usize,
    eew2: usize,
    pow2: i32,
    nf: usize,
) -> bool {
    if eew1 == eew2 {
        valid_reg_group(rs1, pow1) && valid_reg_group(rs2, pow2)
    } else {
        valid_disjoint_segmented(rs1, rs2, pow1, pow2, nf)
    }
}

const fn access_size(bytes: usize) -> AccessSize {
    match bytes {
        1 => AccessSize::Byte,
        2 => AccessSize::Half,
        4 => AccessSize::Word,
        _ => AccessSize::Double,
    }
}

/// Reserved register usage of single-width arithmetic and merge/move
/// instructions (RVV 1.0 sections 3.4.2 and 5.3; Sail `illegal_normal`,
/// `illegal_vd_masked`, `illegal_vd_unmasked`, `valid_reg_group`):
/// operand groups must be aligned to LMUL, and a masked instruction may not
/// write v0. Widening, narrowing, mask-producing, reduction and permutation
/// instructions have other rules and are not checked here.
fn single_width_reserved(
    f3: u32,
    funct6: u32,
    vm: u32,
    vd: usize,
    vs1: usize,
    vs2: usize,
    lmul: i32,
) -> bool {
    let vector_vs1 = matches!(f3, 0..=2);
    let normal = match f3 {
        // OPIVV, OPIVI, OPIVX
        0 | 3 | 4 => matches!(
            funct6,
            0x00 | 0x02 | 0x03 | 0x04..=0x07 | 0x09..=0x0b | 0x20..=0x23 | 0x25 | 0x27..=0x2b
        ),
        // OPMVV, OPMVX
        2 | 6 => matches!(
            funct6,
            0x08..=0x0b | 0x20..=0x27 | 0x29 | 0x2b | 0x2d | 0x2f
        ),
        // OPFVV, OPFVF
        1 | 5 => matches!(
            funct6,
            0x00 | 0x02 | 0x04 | 0x06 | 0x08..=0x0a | 0x20 | 0x21 | 0x24 | 0x27..=0x2f
        ),
        _ => false,
    };
    if normal {
        return (vm == 0 && vd == 0)
            || !valid_reg_group(vd, lmul)
            || !valid_reg_group(vs2, lmul)
            || (vector_vs1 && !valid_reg_group(vs1, lmul));
    }
    // vmerge / vmv.v.{v,x,i} (OPIV*) and vfmerge / vfmv.v.f (OPFVF).
    if funct6 == 0x17 && matches!(f3, 0 | 3 | 4 | 5) {
        let bad_vs1 = f3 == 0 && !valid_reg_group(vs1, lmul);
        return if vm == 0 {
            vd == 0 || !valid_reg_group(vd, lmul) || !valid_reg_group(vs2, lmul) || bad_vs1
        } else {
            vs2 != 0 || !valid_reg_group(vd, lmul) || bad_vs1
        };
    }
    false
}

fn fixed_point_round_shift_u(val: u128, d: u32, vxrm: u64) -> u128 {
    if d == 0 {
        return val;
    }
    let v_d = (val >> d) & 1;
    let v_d_minus_1 = (val >> (d - 1)) & 1;
    let low_mask = if d > 1 { (1u128 << (d - 1)) - 1 } else { 0 };
    let v_low_nonzero = u128::from((val & low_mask) != 0);
    let r = match vxrm & 3 {
        0 => v_d_minus_1,                               // rnu
        1 => v_d_minus_1 & (v_low_nonzero | v_d),       // rne
        2 => 0,                                         // rdn
        _ => (1 ^ v_d) & (v_d_minus_1 | v_low_nonzero), // rod
    };
    (val >> d).wrapping_add(r)
}

#[inline]
fn fixed_point_round_shift_s(val: i128, d: u32, vxrm: u64) -> i128 {
    if d == 0 {
        return val;
    }
    let v_d = ((val >> d) & 1) as u128;
    let v_d_minus_1 = ((val >> (d - 1)) & 1) as u128;
    let low_mask = if d > 1 { (1i128 << (d - 1)) - 1 } else { 0 };
    let v_low_nonzero = u128::from((val & low_mask) != 0);
    let r = match vxrm & 3 {
        0 => v_d_minus_1,                               // rnu
        1 => v_d_minus_1 & (v_low_nonzero | v_d),       // rne
        2 => 0,                                         // rdn
        _ => (1 ^ v_d) & (v_d_minus_1 | v_low_nonzero), // rod
    };
    (val >> d).wrapping_add(r as i128)
}

impl Hart {
    #[inline]
    pub(crate) fn v_elem(&self, reg: usize, sew_bytes: usize, idx: usize) -> u64 {
        let byte_off = idx * sew_bytes;
        let r = (reg + (byte_off / 32)) & 31;
        let off = byte_off & 31;
        let b = &self.v[r];
        match sew_bytes {
            1 => u64::from(b[off]),
            2 => u64::from(u16::from_le_bytes([b[off], b[off + 1]])),
            4 => u64::from(u32::from_le_bytes([
                b[off],
                b[off + 1],
                b[off + 2],
                b[off + 3],
            ])),
            _ => u64::from_le_bytes([
                b[off],
                b[off + 1],
                b[off + 2],
                b[off + 3],
                b[off + 4],
                b[off + 5],
                b[off + 6],
                b[off + 7],
            ]),
        }
    }

    #[inline]
    pub(crate) fn v_elem_signed(&self, reg: usize, sew_bytes: usize, idx: usize) -> i64 {
        sext_sew(self.v_elem(reg, sew_bytes, idx), sew_bytes)
    }

    #[inline]
    pub(crate) fn set_v_elem(&mut self, reg: usize, sew_bytes: usize, idx: usize, val: u64) {
        let byte_off = idx * sew_bytes;
        let r = (reg + (byte_off / 32)) & 31;
        let off = byte_off & 31;
        let b = &mut self.v[r];
        match sew_bytes {
            1 => b[off] = val as u8,
            2 => {
                let bytes = (val as u16).to_le_bytes();
                b[off..off + 2].copy_from_slice(&bytes);
            }
            4 => {
                let bytes = (val as u32).to_le_bytes();
                b[off..off + 4].copy_from_slice(&bytes);
            }
            _ => {
                let bytes = val.to_le_bytes();
                b[off..off + 8].copy_from_slice(&bytes);
            }
        }
    }

    #[inline]
    pub(crate) fn v_mask_bit(&self, vm: u32, idx: usize) -> bool {
        if vm != 0 {
            true
        } else {
            ((self.v[0][idx / 8] >> (idx & 7)) & 1) != 0
        }
    }

    #[inline]
    pub(crate) fn v_reg_mask_bit(&self, reg: usize, idx: usize) -> bool {
        ((self.v[reg & 31][idx / 8] >> (idx & 7)) & 1) != 0
    }

    #[inline]
    pub(crate) fn set_v_reg_mask_bit(&mut self, reg: usize, idx: usize, bit: bool) {
        let byte = &mut self.v[reg & 31][idx / 8];
        let mask = 1u8 << (idx & 7);
        if bit {
            *byte |= mask;
        } else {
            *byte &= !mask;
        }
    }

    /// Returns `(sew_bytes, vlmax)` if `vtype` is valid (`vill == 0`).
    #[inline]
    fn vtype_params(&self) -> Option<(usize, usize)> {
        let vt = self.csr.vtype;
        if (vt >> 63) != 0 {
            return None;
        }
        let vlmul = (vt & 7) as usize;
        let vsew = ((vt >> 3) & 7) as usize;
        if vsew > 3 {
            return None;
        }
        let sew_bytes = 1usize << vsew;
        let vlmax = match vlmul {
            0 => 32 / sew_bytes,
            1 => 64 / sew_bytes,
            2 => 128 / sew_bytes,
            3 => 256 / sew_bytes,
            5 if sew_bytes <= 1 => 4 / sew_bytes,
            6 if sew_bytes <= 2 => 8 / sew_bytes,
            7 if sew_bytes <= 4 => 16 / sew_bytes,
            _ => return None,
        };
        Some((sew_bytes, vlmax))
    }

    pub(crate) fn exec_vector(&mut self, bus: &AddressSpace, insn: &Insn) -> Result<(), Exception> {
        let illegal = || Exception::new(cause::ILLEGAL_INSN, u64::from(insn.raw));
        if (self.csr.mstatus & status::VS) == 0 {
            return Err(illegal());
        }
        match insn.op {
            Op::Vload => self.exec_vload(bus, insn),
            Op::Vstore => self.exec_vstore(bus, insn),
            Op::Vop => self.exec_vop(insn),
            _ => Err(illegal()),
        }
    }

    fn exec_vload(&mut self, bus: &AddressSpace, insn: &Insn) -> Result<(), Exception> {
        let illegal = || Exception::new(cause::ILLEGAL_INSN, u64::from(insn.raw));
        let raw = insn.raw;
        let vd = insn.rd as usize;
        let rs1 = insn.rs1 as usize;
        let rs2 = insn.rs2 as usize;
        let width = (raw >> 12) & 7;
        let vm = (raw >> 25) & 1;
        let mop = (raw >> 26) & 3;
        let nf = (((raw >> 29) & 7) as usize) + 1;
        let eew_bytes: usize = match width {
            0 => 1,
            5 => 2,
            6 => 4,
            7 => 8,
            _ => return Err(illegal()),
        };
        let base_addr = self.x[rs1];

        // 1. Whole-register load (vl<nf>re<eew>.v: mop == 0, lumop == 0x08).
        if mop == 0 && rs2 == 0x08 {
            if vm == 0 || !matches!(nf, 1 | 2 | 4 | 8) || (vd % nf) != 0 {
                return Err(illegal());
            }
            let access_size = match eew_bytes {
                1 => AccessSize::Byte,
                2 => AccessSize::Half,
                4 => AccessSize::Word,
                _ => AccessSize::Double,
            };
            let total_elems = (nf * 32) / eew_bytes;
            let start = self.csr.vstart as usize;
            for i in start..total_elems {
                let addr = base_addr.wrapping_add((i * eew_bytes) as u64);
                let val = self.load(bus, addr, access_size).inspect_err(|_| {
                    self.vector_trap_at(start, i);
                })?;
                self.set_v_elem(vd, eew_bytes, i, val);
            }
            self.csr.vstart = 0;
            self.csr.mstatus |= status::VS;
            return Ok(());
        }

        // 2. Unit-stride mask load (vlm.v: mop == 0, lumop == 0x0b).
        if mop == 0 && rs2 == 0x0b {
            if vm == 0 || nf != 1 || eew_bytes != 1 {
                return Err(illegal());
            }
            let Some(_) = self.vtype_params() else {
                return Err(illegal());
            };
            let evl = (self.csr.vl as usize).div_ceil(8);
            let start = self.csr.vstart as usize;
            for i in start..evl {
                let addr = base_addr.wrapping_add(i as u64);
                let val = self
                    .load(bus, addr, AccessSize::Byte)
                    .inspect_err(|_| self.vector_trap_at(start, i))?;
                self.set_v_elem(vd, 1, i, val);
            }
            self.csr.vstart = 0;
            self.csr.mstatus |= status::VS;
            return Ok(());
        }

        // 3. Unit-stride, fault-only-first, strided, and indexed loads.
        let Some((sew_bytes, _)) = self.vtype_params() else {
            return Err(illegal());
        };
        let lmul = lmul_pow(self.csr.vtype).ok_or_else(illegal)?;
        let is_ff = mop == 0 && rs2 == 0x10;
        if mop == 0 && !matches!(rs2, 0x00 | 0x10) {
            return Err(illegal());
        }
        let indexed = matches!(mop, 1 | 3);
        // Data EEW/EMUL: SEW/LMUL for indexed accesses, otherwise the encoded
        // width with EMUL = (EEW / SEW) * LMUL. Segment field `f` occupies the
        // register group starting at `vd + f * ceil(EMUL)` (RVV 1.0 7.8).
        let (data_bytes, data_pow) = if indexed {
            (sew_bytes, lmul)
        } else {
            (
                eew_bytes,
                width_pow(eew_bytes) - width_pow(sew_bytes) + lmul,
            )
        };
        // Reserved encodings, checked in the same way as the Sail model.
        let reserved = if indexed {
            let index_pow = width_pow(eew_bytes) - width_pow(sew_bytes) + lmul;
            !(-3..=3).contains(&index_pow)
                || (vm == 0 && vd == 0)
                || (vm == 0 && rs2 == 0)
                || !valid_segment(vd, nf, data_pow)
                || if nf == 1 {
                    !valid_reg_overlap(rs2, vd, index_pow, data_pow)
                } else {
                    !valid_disjoint_segmented(rs2, vd, index_pow, data_pow, nf)
                }
        } else {
            !(-3..=3).contains(&data_pow)
                || (vm == 0 && vd == 0)
                || !valid_segment(vd, nf, data_pow)
                || !valid_reg_group(vd, data_pow)
        };
        if reserved {
            return Err(illegal());
        }
        let field_regs = group_regs(data_pow);
        let data_size = access_size(data_bytes);
        let stride = self.x[rs2] as i64;
        let vl = self.csr.vl as usize;
        let start = self.csr.vstart as usize;

        for i in start..vl {
            if !self.v_mask_bit(vm, i) {
                continue;
            }
            let elem_base = match mop {
                0 => base_addr.wrapping_add(((i * nf) * data_bytes) as u64),
                2 => base_addr.wrapping_add((i as i64).wrapping_mul(stride) as u64),
                _ => base_addr.wrapping_add(self.v_elem(rs2, eew_bytes, i)),
            };
            for seg in 0..nf {
                let addr = elem_base.wrapping_add((seg * data_bytes) as u64);
                match self.load(bus, addr, data_size) {
                    Ok(val) => {
                        self.set_v_elem(vd + seg * field_regs, data_bytes, i, val);
                    }
                    Err(e) => {
                        if is_ff && i > 0 {
                            self.csr.vl = i as u64;
                            self.csr.vstart = 0;
                            self.csr.mstatus |= status::VS;
                            return Ok(());
                        }
                        self.vector_trap_at(start, i);
                        return Err(e);
                    }
                }
            }
        }
        self.csr.vstart = 0;
        self.csr.mstatus |= status::VS;
        Ok(())
    }

    /// Records a synchronous exception on element `i` of a vector memory
    /// access that started at element `start`: `vstart` names the faulting
    /// element (RVV 1.0 3.7) so the access can resume after the trap, and
    /// `mstatus.VS` becomes dirty if earlier elements were already written.
    fn vector_trap_at(&mut self, start: usize, i: usize) {
        self.csr.vstart = i as u64;
        if i > start {
            self.csr.mstatus |= status::VS;
        }
    }

    fn exec_vstore(&mut self, bus: &AddressSpace, insn: &Insn) -> Result<(), Exception> {
        let illegal = || Exception::new(cause::ILLEGAL_INSN, u64::from(insn.raw));
        let raw = insn.raw;
        let vs3 = insn.rd as usize;
        let rs1 = insn.rs1 as usize;
        let rs2 = insn.rs2 as usize;
        let width = (raw >> 12) & 7;
        let vm = (raw >> 25) & 1;
        let mop = (raw >> 26) & 3;
        let nf = (((raw >> 29) & 7) as usize) + 1;
        let eew_bytes: usize = match width {
            0 => 1,
            5 => 2,
            6 => 4,
            7 => 8,
            _ => return Err(illegal()),
        };
        let base_addr = self.x[rs1];

        // 1. Whole-register store (vs<nf>r.v: mop == 0, sumop == 0x08).
        if mop == 0 && rs2 == 0x08 {
            if vm == 0 || eew_bytes != 1 || !matches!(nf, 1 | 2 | 4 | 8) || (vs3 % nf) != 0 {
                return Err(illegal());
            }
            let total_bytes = nf * 32;
            let start = self.csr.vstart as usize;
            for i in start..total_bytes {
                let addr = base_addr.wrapping_add(i as u64);
                let val = self.v_elem(vs3, 1, i);
                self.store(bus, addr, AccessSize::Byte, val)
                    .inspect_err(|_| self.csr.vstart = i as u64)?;
            }
            self.csr.vstart = 0;
            return Ok(());
        }

        // 2. Unit-stride mask store (vsm.v: mop == 0, sumop == 0x0b).
        if mop == 0 && rs2 == 0x0b {
            if vm == 0 || nf != 1 || eew_bytes != 1 {
                return Err(illegal());
            }
            let Some(_) = self.vtype_params() else {
                return Err(illegal());
            };
            let evl = (self.csr.vl as usize).div_ceil(8);
            let start = self.csr.vstart as usize;
            for i in start..evl {
                let addr = base_addr.wrapping_add(i as u64);
                let val = self.v_elem(vs3, 1, i);
                self.store(bus, addr, AccessSize::Byte, val)
                    .inspect_err(|_| self.csr.vstart = i as u64)?;
            }
            self.csr.vstart = 0;
            return Ok(());
        }

        // 3. Unit-stride, strided, and indexed stores.
        let Some((sew_bytes, _)) = self.vtype_params() else {
            return Err(illegal());
        };
        let lmul = lmul_pow(self.csr.vtype).ok_or_else(illegal)?;
        if mop == 0 && rs2 != 0x00 {
            return Err(illegal());
        }
        let indexed = matches!(mop, 1 | 3);
        let (data_bytes, data_pow) = if indexed {
            (sew_bytes, lmul)
        } else {
            (
                eew_bytes,
                width_pow(eew_bytes) - width_pow(sew_bytes) + lmul,
            )
        };
        let reserved = if indexed {
            let index_pow = width_pow(eew_bytes) - width_pow(sew_bytes) + lmul;
            !(-3..=3).contains(&index_pow)
                || !valid_segment(vs3, nf, data_pow)
                || !valid_src_overlap_segmented(
                    rs2, eew_bytes, index_pow, vs3, sew_bytes, data_pow, nf,
                )
                || (vm == 0 && (rs2 == 0 || vs3 == 0))
        } else {
            !(-3..=3).contains(&data_pow)
                || !valid_segment(vs3, nf, data_pow)
                || !valid_reg_group(vs3, data_pow)
                || (vm == 0 && vs3 == 0)
        };
        if reserved {
            return Err(illegal());
        }
        let field_regs = group_regs(data_pow);
        let data_size = access_size(data_bytes);
        let stride = self.x[rs2] as i64;
        let vl = self.csr.vl as usize;
        let start = self.csr.vstart as usize;

        for i in start..vl {
            if !self.v_mask_bit(vm, i) {
                continue;
            }
            let elem_base = match mop {
                0 => base_addr.wrapping_add(((i * nf) * data_bytes) as u64),
                2 => base_addr.wrapping_add((i as i64).wrapping_mul(stride) as u64),
                _ => base_addr.wrapping_add(self.v_elem(rs2, eew_bytes, i)),
            };
            for seg in 0..nf {
                let addr = elem_base.wrapping_add((seg * data_bytes) as u64);
                let val = self.v_elem(vs3 + seg * field_regs, data_bytes, i);
                self.store(bus, addr, data_size, val)
                    .inspect_err(|_| self.csr.vstart = i as u64)?;
            }
        }
        self.csr.vstart = 0;
        Ok(())
    }

    fn exec_vop(&mut self, insn: &Insn) -> Result<(), Exception> {
        let illegal = || Exception::new(cause::ILLEGAL_INSN, u64::from(insn.raw));
        let raw = insn.raw;
        let f3 = (raw >> 12) & 7;
        let vd = insn.rd as usize;
        let rs1 = insn.rs1 as usize;
        let rs2 = insn.rs2 as usize;

        // 1. Vector configuration instructions (f3 == 7: vsetvli, vsetivli, vsetvl).
        if f3 == 7 {
            let zimm = if (raw >> 31) == 0 {
                u64::from((raw >> 20) & 0x7ff)
            } else if (raw >> 30) == 0b11 {
                u64::from((raw >> 20) & 0x3ff)
            } else if (raw >> 25) == 0b1000000 {
                self.x[rs2]
            } else {
                return Err(illegal());
            };
            let vlmul = (zimm & 7) as usize;
            let vsew = ((zimm >> 3) & 7) as usize;
            let reserved = zimm >> 8;
            let sew_bytes = 1usize.checked_shl(vsew as u32).unwrap_or(usize::MAX);
            let vlmax_opt = if reserved != 0 || vsew > 3 {
                None
            } else {
                match vlmul {
                    0 => Some(32 / sew_bytes),
                    1 => Some(64 / sew_bytes),
                    2 => Some(128 / sew_bytes),
                    3 => Some(256 / sew_bytes),
                    5 if sew_bytes <= 1 => Some(4 / sew_bytes),
                    6 if sew_bytes <= 2 => Some(8 / sew_bytes),
                    7 if sew_bytes <= 4 => Some(16 / sew_bytes),
                    _ => None,
                }
            };
            // `vsetvl{i} x0, x0, vtype` keeps vl and is reserved when the
            // SEW/LMUL ratio would change or vtype was invalid; set vill
            // in that case, as the Sail model does (RVV 1.0 6.2).
            let keeps_vl = (raw >> 30) != 0b11 && rs1 == 0 && vd == 0;
            let old_vtype = self.csr.vtype;
            let ratio_kept = match (lmul_pow(old_vtype), lmul_pow(zimm)) {
                (Some(old_lmul), Some(new_lmul)) if old_vtype >> 63 == 0 => {
                    old_lmul - ((old_vtype >> 3) & 7) as i32 == new_lmul - vsew as i32
                }
                _ => false,
            };
            let vlmax_opt = if keeps_vl && !ratio_kept {
                None
            } else {
                vlmax_opt
            };
            let Some(vlmax) = vlmax_opt else {
                self.csr.vtype = 1 << 63;
                self.csr.vl = 0;
                self.csr.vstart = 0;
                self.set_x(insn.rd, 0);
                self.csr.mstatus |= status::VS;
                return Ok(());
            };
            let avl = if (raw >> 30) == 0b11 {
                rs1 as u64
            } else if rs1 != 0 {
                self.x[rs1]
            } else if vd != 0 {
                u64::MAX
            } else {
                self.csr.vl
            };
            let new_vl = avl.min(vlmax as u64);
            self.csr.vtype = zimm & 0xff;
            self.csr.vl = new_vl;
            self.csr.vstart = 0;
            self.set_x(insn.rd, new_vl);
            self.csr.mstatus |= status::VS;
            return Ok(());
        }

        let funct6 = raw >> 26;
        let vm = (raw >> 25) & 1;

        // 2. Whole-register move (vmv<nr>r.v: f3 == 3, funct6 == 0x27, vm == 1) works even when vill == 1.
        if f3 == 3 && funct6 == 0x27 && vm == 1 {
            let nregs = rs1 + 1;
            if !matches!(nregs, 1 | 2 | 4 | 8) || (vd % nregs) != 0 || (rs2 % nregs) != 0 {
                return Err(illegal());
            }
            for r in 0..nregs {
                self.v[vd + r] = self.v[rs2 + r];
            }
            self.csr.vstart = 0;
            self.csr.mstatus |= status::VS;
            return Ok(());
        }

        let Some((sew_bytes, vlmax)) = self.vtype_params() else {
            return Err(illegal());
        };
        if self.csr.vstart != 0 {
            return Err(illegal());
        }
        let lmul = lmul_pow(self.csr.vtype).ok_or_else(illegal)?;
        if single_width_reserved(f3, funct6, vm, vd, rs1, rs2, lmul) {
            return Err(illegal());
        }

        match f3 {
            0 | 3 | 4 => self.exec_vop_int(insn, f3, funct6, vm, vd, rs1, rs2, sew_bytes, vlmax),
            2 | 6 => self.exec_vop_mv(insn, f3, funct6, vm, vd, rs1, rs2, sew_bytes, vlmax),
            1 | 5 => self.exec_vop_fp(insn, f3, funct6, vm, vd, rs1, rs2, sew_bytes),
            _ => Err(illegal()),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn exec_vop_int(
        &mut self,
        insn: &Insn,
        f3: u32,
        funct6: u32,
        vm: u32,
        vd: usize,
        rs1: usize,
        vs2: usize,
        sew_bytes: usize,
        vlmax: usize,
    ) -> Result<(), Exception> {
        let illegal = || Exception::new(cause::ILLEGAL_INSN, u64::from(insn.raw));
        let vl = self.csr.vl as usize;
        let sew_bits = (sew_bytes * 8) as u32;
        let simm5 = ((insn.rs1 as i8) << 3) >> 3;
        let uimm5 = u64::from(insn.rs1);
        let vxrm = self.csr.vxrm;

        // Snapshot source vector registers so in-place overlap works cleanly.
        let old_v = self.v;
        let x_rs1 = self.x[rs1];
        let get_vs2 = |idx: usize, width: usize| -> u64 {
            let byte_off = idx * width;
            let r = (vs2 + (byte_off / 32)) & 31;
            let off = byte_off & 31;
            let b = &old_v[r];
            match width {
                1 => u64::from(b[off]),
                2 => u64::from(u16::from_le_bytes([b[off], b[off + 1]])),
                4 => u64::from(u32::from_le_bytes([
                    b[off],
                    b[off + 1],
                    b[off + 2],
                    b[off + 3],
                ])),
                _ => u64::from_le_bytes([
                    b[off],
                    b[off + 1],
                    b[off + 2],
                    b[off + 3],
                    b[off + 4],
                    b[off + 5],
                    b[off + 6],
                    b[off + 7],
                ]),
            }
        };
        let get_vs1 = |idx: usize, width: usize| -> u64 {
            match f3 {
                0 => {
                    let byte_off = idx * width;
                    let r = (rs1 + (byte_off / 32)) & 31;
                    let off = byte_off & 31;
                    let b = &old_v[r];
                    match width {
                        1 => u64::from(b[off]),
                        2 => u64::from(u16::from_le_bytes([b[off], b[off + 1]])),
                        4 => u64::from(u32::from_le_bytes([
                            b[off],
                            b[off + 1],
                            b[off + 2],
                            b[off + 3],
                        ])),
                        _ => u64::from_le_bytes([
                            b[off],
                            b[off + 1],
                            b[off + 2],
                            b[off + 3],
                            b[off + 4],
                            b[off + 5],
                            b[off + 6],
                            b[off + 7],
                        ]),
                    }
                }
                4 => mask_sew(x_rs1, width),
                _ => mask_sew(simm5 as i64 as u64, width),
            }
        };
        let get_vs1_uimm =
            |idx: usize, width: usize| -> u64 { if f3 == 3 { uimm5 } else { get_vs1(idx, width) } };

        // Widening sum reductions: vwredsumu.vs (0x30) and vwredsum.vs (0x31).
        if matches!(funct6, 0x30 | 0x31) && f3 == 0 {
            if sew_bytes >= 8 {
                return Err(illegal());
            }
            let wide_bytes = sew_bytes * 2;
            if vl > 0 {
                let mut acc = self.v_elem(rs1, wide_bytes, 0);
                for i in 0..vl {
                    if self.v_mask_bit(vm, i) {
                        let elem = if funct6 == 0x30 {
                            get_vs2(i, sew_bytes)
                        } else {
                            sext_sew(get_vs2(i, sew_bytes), sew_bytes) as u64
                        };
                        acc = mask_sew(acc.wrapping_add(elem), wide_bytes);
                    }
                }
                self.set_v_elem(vd, wide_bytes, 0, acc);
            }
            self.csr.mstatus |= status::VS;
            return Ok(());
        }

        for i in 0..vl {
            let active = self.v_mask_bit(vm, i);
            let a = get_vs2(i, sew_bytes);
            let b = get_vs1(i, sew_bytes);
            let a_s = sext_sew(a, sew_bytes);
            let b_s = sext_sew(b, sew_bytes);

            match funct6 {
                0x00 => {
                    if active {
                        self.set_v_elem(vd, sew_bytes, i, a.wrapping_add(b));
                    }
                }
                0x02 => {
                    if active {
                        self.set_v_elem(vd, sew_bytes, i, a.wrapping_sub(b));
                    }
                }
                0x03 if f3 != 0 => {
                    if active {
                        self.set_v_elem(vd, sew_bytes, i, b.wrapping_sub(a));
                    }
                }
                0x04 if f3 != 3 => {
                    if active {
                        self.set_v_elem(vd, sew_bytes, i, a.min(b));
                    }
                }
                0x05 if f3 != 3 => {
                    if active {
                        self.set_v_elem(vd, sew_bytes, i, a_s.min(b_s) as u64);
                    }
                }
                0x06 if f3 != 3 => {
                    if active {
                        self.set_v_elem(vd, sew_bytes, i, a.max(b));
                    }
                }
                0x07 if f3 != 3 => {
                    if active {
                        self.set_v_elem(vd, sew_bytes, i, a_s.max(b_s) as u64);
                    }
                }
                0x09 => {
                    if active {
                        self.set_v_elem(vd, sew_bytes, i, a & b);
                    }
                }
                0x0a => {
                    if active {
                        self.set_v_elem(vd, sew_bytes, i, a | b);
                    }
                }
                0x0b => {
                    if active {
                        self.set_v_elem(vd, sew_bytes, i, a ^ b);
                    }
                }
                0x0c => {
                    // vrgather.vv / vrgather.vx / vrgather.vi
                    if active {
                        let idx = if f3 == 3 {
                            uimm5 as usize
                        } else if f3 == 4 {
                            self.x[rs1] as usize
                        } else {
                            b as usize
                        };
                        let val = if idx >= vlmax {
                            0
                        } else {
                            get_vs2(idx, sew_bytes)
                        };
                        self.set_v_elem(vd, sew_bytes, i, val);
                    }
                }
                0x0e => {
                    if f3 == 0 {
                        // vrgatherei16.vv
                        if active {
                            let idx = get_vs1(i, 2) as usize;
                            let val = if idx >= vlmax {
                                0
                            } else {
                                get_vs2(idx, sew_bytes)
                            };
                            self.set_v_elem(vd, sew_bytes, i, val);
                        }
                    } else {
                        // vslideup.vx / vslideup.vi (elements < offset are always undisturbed!)
                        let offset = if f3 == 3 {
                            uimm5 as usize
                        } else {
                            self.x[rs1] as usize
                        };
                        if i >= offset && active {
                            let val = get_vs2(i - offset, sew_bytes);
                            self.set_v_elem(vd, sew_bytes, i, val);
                        }
                    }
                }
                0x0f if f3 != 0 => {
                    // vslidedown.vx / vslidedown.vi
                    if active {
                        let offset = if f3 == 3 {
                            uimm5 as usize
                        } else {
                            self.x[rs1] as usize
                        };
                        let src_idx = i.saturating_add(offset);
                        let val = if src_idx < vlmax {
                            get_vs2(src_idx, sew_bytes)
                        } else {
                            0
                        };
                        self.set_v_elem(vd, sew_bytes, i, val);
                    }
                }
                0x10 if vm == 0 => {
                    // vadc.vvm / vadc.vxm / vadc.vim
                    let cin = u64::from(self.v_reg_mask_bit(0, i));
                    let sum = a.wrapping_add(b).wrapping_add(cin);
                    self.set_v_elem(vd, sew_bytes, i, sum);
                }
                0x11 => {
                    // vmadc.vvm / vmadc.vxm / vmadc.vim / vmadc.vv / vmadc.vx / vmadc.vi
                    let cin = if vm == 0 {
                        u128::from(self.v_reg_mask_bit(0, i))
                    } else {
                        0
                    };
                    let sum = u128::from(a) + u128::from(b) + cin;
                    let cout = (sum >> sew_bits) != 0;
                    self.set_v_reg_mask_bit(vd, i, cout);
                }
                0x12 if vm == 0 && f3 != 3 => {
                    // vsbc.vvm / vsbc.vxm
                    let bin = u64::from(self.v_reg_mask_bit(0, i));
                    let diff = a.wrapping_sub(b).wrapping_sub(bin);
                    self.set_v_elem(vd, sew_bytes, i, diff);
                }
                0x13 if f3 != 3 => {
                    // vmsbc.vvm / vmsbc.vxm / vmsbc.vv / vmsbc.vx
                    let bin = if vm == 0 {
                        u128::from(self.v_reg_mask_bit(0, i))
                    } else {
                        0
                    };
                    let borrow = u128::from(a) < (u128::from(b) + bin);
                    self.set_v_reg_mask_bit(vd, i, borrow);
                }
                0x17 => {
                    // vmerge.vvm / vmerge.vxm / vmerge.vim (vm == 0) or vmv.v.v / vmv.v.x / vmv.v.i (vm == 1, vs2 == 0)
                    if vm == 1 {
                        if vs2 != 0 {
                            return Err(illegal());
                        }
                        self.set_v_elem(vd, sew_bytes, i, b);
                    } else {
                        let cond = self.v_reg_mask_bit(0, i);
                        self.set_v_elem(vd, sew_bytes, i, if cond { b } else { a });
                    }
                }
                0x18 => {
                    if active {
                        self.set_v_reg_mask_bit(vd, i, a == b);
                    }
                }
                0x19 => {
                    if active {
                        self.set_v_reg_mask_bit(vd, i, a != b);
                    }
                }
                0x1a if f3 != 3 => {
                    if active {
                        self.set_v_reg_mask_bit(vd, i, a < b);
                    }
                }
                0x1b if f3 != 3 => {
                    if active {
                        self.set_v_reg_mask_bit(vd, i, a_s < b_s);
                    }
                }
                0x1c => {
                    if active {
                        self.set_v_reg_mask_bit(vd, i, a <= b);
                    }
                }
                0x1d => {
                    if active {
                        self.set_v_reg_mask_bit(vd, i, a_s <= b_s);
                    }
                }
                0x1e if f3 != 0 => {
                    if active {
                        self.set_v_reg_mask_bit(vd, i, a > b);
                    }
                }
                0x1f if f3 != 0 => {
                    if active {
                        self.set_v_reg_mask_bit(vd, i, a_s > b_s);
                    }
                }
                0x20 => {
                    // vsaddu
                    if active {
                        let max_u = if sew_bits == 64 {
                            u128::from(u64::MAX)
                        } else {
                            (1u128 << sew_bits) - 1
                        };
                        let sum = u128::from(a) + u128::from(b);
                        let res = if sum > max_u {
                            self.csr.vxsat = 1;
                            max_u as u64
                        } else {
                            sum as u64
                        };
                        self.set_v_elem(vd, sew_bytes, i, res);
                    }
                }
                0x21 => {
                    // vsadd
                    if active {
                        let max_s = (1i128 << (sew_bits - 1)) - 1;
                        let min_s = -(1i128 << (sew_bits - 1));
                        let sum = i128::from(a_s) + i128::from(b_s);
                        let res = if sum > max_s {
                            self.csr.vxsat = 1;
                            max_s as i64 as u64
                        } else if sum < min_s {
                            self.csr.vxsat = 1;
                            min_s as i64 as u64
                        } else {
                            sum as i64 as u64
                        };
                        self.set_v_elem(vd, sew_bytes, i, res);
                    }
                }
                0x22 if f3 != 3 => {
                    // vssubu
                    if active {
                        let res = if a < b {
                            self.csr.vxsat = 1;
                            0
                        } else {
                            a - b
                        };
                        self.set_v_elem(vd, sew_bytes, i, res);
                    }
                }
                0x23 if f3 != 3 => {
                    // vssub
                    if active {
                        let max_s = (1i128 << (sew_bits - 1)) - 1;
                        let min_s = -(1i128 << (sew_bits - 1));
                        let diff = i128::from(a_s) - i128::from(b_s);
                        let res = if diff > max_s {
                            self.csr.vxsat = 1;
                            max_s as i64 as u64
                        } else if diff < min_s {
                            self.csr.vxsat = 1;
                            min_s as i64 as u64
                        } else {
                            diff as i64 as u64
                        };
                        self.set_v_elem(vd, sew_bytes, i, res);
                    }
                }
                0x25 => {
                    // vsll
                    if active {
                        let shamt = (get_vs1_uimm(i, sew_bytes) as u32) & (sew_bits - 1);
                        self.set_v_elem(vd, sew_bytes, i, a << shamt);
                    }
                }
                0x27 if f3 != 3 => {
                    // vsmul.vv / vsmul.vx
                    if active {
                        let min_s = -(1i128 << (sew_bits - 1));
                        let max_s = (1i128 << (sew_bits - 1)) - 1;
                        let res = if i128::from(a_s) == min_s && i128::from(b_s) == min_s {
                            self.csr.vxsat = 1;
                            max_s as i64 as u64
                        } else {
                            let prod = i128::from(a_s) * i128::from(b_s);
                            let rounded = fixed_point_round_shift_s(prod, sew_bits - 1, vxrm);
                            rounded as i64 as u64
                        };
                        self.set_v_elem(vd, sew_bytes, i, res);
                    }
                }
                0x28 => {
                    // vsrl
                    if active {
                        let shamt = (get_vs1_uimm(i, sew_bytes) as u32) & (sew_bits - 1);
                        self.set_v_elem(vd, sew_bytes, i, a >> shamt);
                    }
                }
                0x29 => {
                    // vsra
                    if active {
                        let shamt = (get_vs1_uimm(i, sew_bytes) as u32) & (sew_bits - 1);
                        self.set_v_elem(vd, sew_bytes, i, (a_s >> shamt) as u64);
                    }
                }
                0x2a => {
                    // vssrl
                    if active {
                        let shamt = (get_vs1_uimm(i, sew_bytes) as u32) & (sew_bits - 1);
                        let res = fixed_point_round_shift_u(u128::from(a), shamt, vxrm) as u64;
                        self.set_v_elem(vd, sew_bytes, i, res);
                    }
                }
                0x2b => {
                    // vssra
                    if active {
                        let shamt = (get_vs1_uimm(i, sew_bytes) as u32) & (sew_bits - 1);
                        let res = fixed_point_round_shift_s(i128::from(a_s), shamt, vxrm) as u64;
                        self.set_v_elem(vd, sew_bytes, i, res);
                    }
                }
                0x2c => {
                    // vnsrl.wv / vnsrl.wx / vnsrl.wi
                    if sew_bytes >= 8 {
                        return Err(illegal());
                    }
                    if active {
                        let wide = get_vs2(i, sew_bytes * 2);
                        let shamt = (get_vs1_uimm(i, sew_bytes) as u32) & (sew_bits * 2 - 1);
                        self.set_v_elem(vd, sew_bytes, i, wide >> shamt);
                    }
                }
                0x2d => {
                    // vnsra.wv / vnsra.wx / vnsra.wi
                    if sew_bytes >= 8 {
                        return Err(illegal());
                    }
                    if active {
                        let wide = sext_sew(get_vs2(i, sew_bytes * 2), sew_bytes * 2);
                        let shamt = (get_vs1_uimm(i, sew_bytes) as u32) & (sew_bits * 2 - 1);
                        self.set_v_elem(vd, sew_bytes, i, (wide >> shamt) as u64);
                    }
                }
                0x2e => {
                    // vnclipu.wv / vnclipu.wx / vnclipu.wi
                    if sew_bytes >= 8 {
                        return Err(illegal());
                    }
                    if active {
                        let wide = u128::from(get_vs2(i, sew_bytes * 2));
                        let shamt = (get_vs1_uimm(i, sew_bytes) as u32) & (sew_bits * 2 - 1);
                        let rounded = fixed_point_round_shift_u(wide, shamt, vxrm);
                        let max_u = (1u128 << sew_bits) - 1;
                        let res = if rounded > max_u {
                            self.csr.vxsat = 1;
                            max_u as u64
                        } else {
                            rounded as u64
                        };
                        self.set_v_elem(vd, sew_bytes, i, res);
                    }
                }
                0x2f => {
                    // vnclip.wv / vnclip.wx / vnclip.wi
                    if sew_bytes >= 8 {
                        return Err(illegal());
                    }
                    if active {
                        let wide = i128::from(sext_sew(get_vs2(i, sew_bytes * 2), sew_bytes * 2));
                        let shamt = (get_vs1_uimm(i, sew_bytes) as u32) & (sew_bits * 2 - 1);
                        let rounded = fixed_point_round_shift_s(wide, shamt, vxrm);
                        let max_s = (1i128 << (sew_bits - 1)) - 1;
                        let min_s = -(1i128 << (sew_bits - 1));
                        let res = if rounded > max_s {
                            self.csr.vxsat = 1;
                            max_s as i64 as u64
                        } else if rounded < min_s {
                            self.csr.vxsat = 1;
                            min_s as i64 as u64
                        } else {
                            rounded as i64 as u64
                        };
                        self.set_v_elem(vd, sew_bytes, i, res);
                    }
                }
                _ => return Err(illegal()),
            }
        }
        self.csr.mstatus |= status::VS;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn exec_vop_mv(
        &mut self,
        insn: &Insn,
        f3: u32,
        funct6: u32,
        vm: u32,
        vd: usize,
        rs1: usize,
        vs2: usize,
        sew_bytes: usize,
        _vlmax: usize,
    ) -> Result<(), Exception> {
        let illegal = || Exception::new(cause::ILLEGAL_INSN, u64::from(insn.raw));
        let vl = self.csr.vl as usize;
        let sew_bits = (sew_bytes * 8) as u32;
        let vxrm = self.csr.vxrm;

        // 1. Reductions (OPMVV: f3 == 2, funct6 in 0x00..=0x07).
        if f3 == 2 && funct6 <= 0x07 {
            if vl > 0 {
                let mut acc = self.v_elem(rs1, sew_bytes, 0);
                for i in 0..vl {
                    if self.v_mask_bit(vm, i) {
                        let v = self.v_elem(vs2, sew_bytes, i);
                        acc = match funct6 {
                            0x00 => mask_sew(acc.wrapping_add(v), sew_bytes),
                            0x01 => acc & v,
                            0x02 => acc | v,
                            0x03 => acc ^ v,
                            0x04 => acc.min(v),
                            0x05 => {
                                sext_sew(acc, sew_bytes).min(sext_sew(v, sew_bytes)) as u64
                                    & mask_sew(u64::MAX, sew_bytes)
                            }
                            0x06 => acc.max(v),
                            _ => {
                                sext_sew(acc, sew_bytes).max(sext_sew(v, sew_bytes)) as u64
                                    & mask_sew(u64::MAX, sew_bytes)
                            }
                        };
                    }
                }
                self.set_v_elem(vd, sew_bytes, 0, acc);
            }
            self.csr.mstatus |= status::VS;
            return Ok(());
        }

        // 2. VWXUNARY0 (f3 == 2, funct6 == 0x10): vmv.x.s, vcpop.m, vfirst.m.
        if f3 == 2 && funct6 == 0x10 {
            match rs1 {
                0 if vm == 1 => {
                    // vmv.x.s rd, vs2
                    let val = self.v_elem_signed(vs2, sew_bytes, 0) as u64;
                    self.set_x(insn.rd, val);
                }
                16 => {
                    // vcpop.m rd, vs2, vm
                    let mut count = 0u64;
                    for i in 0..vl {
                        if self.v_mask_bit(vm, i) && self.v_reg_mask_bit(vs2, i) {
                            count += 1;
                        }
                    }
                    self.set_x(insn.rd, count);
                }
                17 => {
                    // vfirst.m rd, vs2, vm
                    let mut first = u64::MAX;
                    for i in 0..vl {
                        if self.v_mask_bit(vm, i) && self.v_reg_mask_bit(vs2, i) {
                            first = i as u64;
                            break;
                        }
                    }
                    self.set_x(insn.rd, first);
                }
                _ => return Err(illegal()),
            }
            return Ok(());
        }

        // 3. VRXUNARY0 (f3 == 6, funct6 == 0x10): vmv.s.x.
        if f3 == 6 && funct6 == 0x10 {
            if vs2 != 0 {
                return Err(illegal());
            }
            if vl > 0 {
                self.set_v_elem(vd, sew_bytes, 0, self.x[rs1]);
            }
            self.csr.mstatus |= status::VS;
            return Ok(());
        }

        // 4. VXUNARY0 (f3 == 2, funct6 == 0x12): vzext.vf{2,4,8}, vsext.vf{2,4,8}.
        if f3 == 2 && funct6 == 0x12 {
            let (div, signed) = match rs1 {
                2 => (8usize, false),
                3 => (8usize, true),
                4 => (4usize, false),
                5 => (4usize, true),
                6 => (2usize, false),
                7 => (2usize, true),
                _ => return Err(illegal()),
            };
            if sew_bytes < div {
                return Err(illegal());
            }
            let src_bytes = sew_bytes / div;
            let old_v = self.v;
            for i in 0..vl {
                if self.v_mask_bit(vm, i) {
                    let byte_off = i * src_bytes;
                    let r = (vs2 + (byte_off / 32)) & 31;
                    let off = byte_off & 31;
                    let b = &old_v[r];
                    let raw_elem = match src_bytes {
                        1 => u64::from(b[off]),
                        2 => u64::from(u16::from_le_bytes([b[off], b[off + 1]])),
                        _ => u64::from(u32::from_le_bytes([
                            b[off],
                            b[off + 1],
                            b[off + 2],
                            b[off + 3],
                        ])),
                    };
                    let val = if signed {
                        sext_sew(raw_elem, src_bytes) as u64
                    } else {
                        raw_elem
                    };
                    self.set_v_elem(vd, sew_bytes, i, val);
                }
            }
            self.csr.mstatus |= status::VS;
            return Ok(());
        }

        // 5. VMUNARY0 (f3 == 2, funct6 == 0x14): vmsbf.m, vmsof.m, vmsif.m, viota.m, vid.v.
        if f3 == 2 && funct6 == 0x14 {
            let old_v = self.v;
            let src_mask_bit =
                |idx: usize| -> bool { ((old_v[vs2 & 31][idx / 8] >> (idx & 7)) & 1) != 0 };
            let vm_bit =
                |idx: usize| -> bool { vm != 0 || ((old_v[0][idx / 8] >> (idx & 7)) & 1) != 0 };
            match rs1 {
                1..=3 => {
                    let mut found_one = false;
                    for i in 0..vl {
                        if vm_bit(i) {
                            let s = src_mask_bit(i);
                            let out = match rs1 {
                                1 => {
                                    // vmsbf.m
                                    if found_one || s {
                                        found_one = true;
                                        false
                                    } else {
                                        true
                                    }
                                }
                                2 => {
                                    // vmsof.m
                                    if !found_one && s {
                                        found_one = true;
                                        true
                                    } else {
                                        false
                                    }
                                }
                                _ => {
                                    // vmsif.m
                                    if found_one {
                                        false
                                    } else {
                                        if s {
                                            found_one = true;
                                        }
                                        true
                                    }
                                }
                            };
                            self.set_v_reg_mask_bit(vd, i, out);
                        }
                    }
                }
                16 => {
                    // viota.m
                    let mut sum = 0u64;
                    for i in 0..vl {
                        if vm_bit(i) {
                            self.set_v_elem(vd, sew_bytes, i, sum);
                            if src_mask_bit(i) {
                                sum += 1;
                            }
                        }
                    }
                }
                17 if vs2 == 0 => {
                    // vid.v
                    for i in 0..vl {
                        if vm_bit(i) {
                            self.set_v_elem(vd, sew_bytes, i, i as u64);
                        }
                    }
                }
                _ => return Err(illegal()),
            }
            self.csr.mstatus |= status::VS;
            return Ok(());
        }

        // 6. vcompress.vm (f3 == 2, funct6 == 0x17, vm == 1).
        if f3 == 2 && funct6 == 0x17 && vm == 1 {
            let old_v = self.v;
            let mut dst_idx = 0usize;
            for i in 0..vl {
                if ((old_v[rs1 & 31][i / 8] >> (i & 7)) & 1) != 0 {
                    let byte_off = i * sew_bytes;
                    let r = (vs2 + (byte_off / 32)) & 31;
                    let off = byte_off & 31;
                    let b = &old_v[r];
                    let val = match sew_bytes {
                        1 => u64::from(b[off]),
                        2 => u64::from(u16::from_le_bytes([b[off], b[off + 1]])),
                        4 => u64::from(u32::from_le_bytes([
                            b[off],
                            b[off + 1],
                            b[off + 2],
                            b[off + 3],
                        ])),
                        _ => u64::from_le_bytes([
                            b[off],
                            b[off + 1],
                            b[off + 2],
                            b[off + 3],
                            b[off + 4],
                            b[off + 5],
                            b[off + 6],
                            b[off + 7],
                        ]),
                    };
                    self.set_v_elem(vd, sew_bytes, dst_idx, val);
                    dst_idx += 1;
                }
            }
            self.csr.mstatus |= status::VS;
            return Ok(());
        }

        // 7. Mask-register logical operations (f3 == 2, funct6 in 0x18..=0x1f, vm == 1).
        if f3 == 2 && (0x18..=0x1f).contains(&funct6) && vm == 1 {
            let old_v = self.v;
            for i in 0..vl {
                let b2 = ((old_v[vs2 & 31][i / 8] >> (i & 7)) & 1) != 0;
                let b1 = ((old_v[rs1 & 31][i / 8] >> (i & 7)) & 1) != 0;
                let out = match funct6 {
                    0x18 => b2 && !b1,   // vmandn.mm
                    0x19 => b2 && b1,    // vmand.mm
                    0x1a => b2 || b1,    // vmor.mm
                    0x1b => b2 ^ b1,     // vmxor.mm
                    0x1c => b2 || !b1,   // vmorn.mm
                    0x1d => !(b2 && b1), // vmnand.mm
                    0x1e => !(b2 || b1), // vmnor.mm
                    _ => !(b2 ^ b1),     // vmxnor.mm
                };
                self.set_v_reg_mask_bit(vd, i, out);
            }
            self.csr.mstatus |= status::VS;
            return Ok(());
        }

        // 8. Element-wise OPMVV / OPMVX operations (averaging, slide1, mul/div/rem/macc, widening).
        let old_v = self.v;
        let get_elem = |reg: usize, idx: usize, width: usize| -> u64 {
            let byte_off = idx * width;
            let r = (reg + (byte_off / 32)) & 31;
            let off = byte_off & 31;
            let b = &old_v[r];
            match width {
                1 => u64::from(b[off]),
                2 => u64::from(u16::from_le_bytes([b[off], b[off + 1]])),
                4 => u64::from(u32::from_le_bytes([
                    b[off],
                    b[off + 1],
                    b[off + 2],
                    b[off + 3],
                ])),
                _ => u64::from_le_bytes([
                    b[off],
                    b[off + 1],
                    b[off + 2],
                    b[off + 3],
                    b[off + 4],
                    b[off + 5],
                    b[off + 6],
                    b[off + 7],
                ]),
            }
        };

        for i in 0..vl {
            if !self.v_mask_bit(vm, i) {
                continue;
            }
            let a = get_elem(vs2, i, sew_bytes);
            let b = if f3 == 2 {
                get_elem(rs1, i, sew_bytes)
            } else {
                mask_sew(self.x[rs1], sew_bytes)
            };
            let a_s = sext_sew(a, sew_bytes);
            let b_s = sext_sew(b, sew_bytes);

            match funct6 {
                0x08 => {
                    // vaaddu
                    let res =
                        fixed_point_round_shift_u(u128::from(a) + u128::from(b), 1, vxrm) as u64;
                    self.set_v_elem(vd, sew_bytes, i, res);
                }
                0x09 => {
                    // vaadd
                    let res = fixed_point_round_shift_s(i128::from(a_s) + i128::from(b_s), 1, vxrm)
                        as u64;
                    self.set_v_elem(vd, sew_bytes, i, res);
                }
                0x0a => {
                    // vasubu
                    let res = fixed_point_round_shift_u(
                        (u128::from(a)).wrapping_sub(u128::from(b)),
                        1,
                        vxrm,
                    ) as u64;
                    self.set_v_elem(vd, sew_bytes, i, res);
                }
                0x0b => {
                    // vasub
                    let res = fixed_point_round_shift_s(i128::from(a_s) - i128::from(b_s), 1, vxrm)
                        as u64;
                    self.set_v_elem(vd, sew_bytes, i, res);
                }
                0x0e if f3 == 6 => {
                    // vslide1up.vx
                    let val = if i == 0 {
                        self.x[rs1]
                    } else {
                        get_elem(vs2, i - 1, sew_bytes)
                    };
                    self.set_v_elem(vd, sew_bytes, i, val);
                }
                0x0f if f3 == 6 => {
                    // vslide1down.vx
                    let val = if i + 1 < vl {
                        get_elem(vs2, i + 1, sew_bytes)
                    } else {
                        self.x[rs1]
                    };
                    self.set_v_elem(vd, sew_bytes, i, val);
                }
                0x20 => {
                    // vdivu
                    let res = a
                        .checked_div(b)
                        .unwrap_or_else(|| mask_sew(u64::MAX, sew_bytes));
                    self.set_v_elem(vd, sew_bytes, i, res);
                }
                0x21 => {
                    // vdiv
                    let min_s = -(1i64 << (sew_bits - 1));
                    let res = if b_s == 0 {
                        -1i64 as u64
                    } else if a_s == min_s && b_s == -1 {
                        min_s as u64
                    } else {
                        (a_s / b_s) as u64
                    };
                    self.set_v_elem(vd, sew_bytes, i, res);
                }
                0x22 => {
                    // vremu
                    let res = if b == 0 { a } else { a % b };
                    self.set_v_elem(vd, sew_bytes, i, res);
                }
                0x23 => {
                    // vrem
                    let min_s = -(1i64 << (sew_bits - 1));
                    let res = if b_s == 0 {
                        a_s as u64
                    } else if a_s == min_s && b_s == -1 {
                        0
                    } else {
                        (a_s % b_s) as u64
                    };
                    self.set_v_elem(vd, sew_bytes, i, res);
                }
                0x24 => {
                    // vmulhu
                    let prod = (u128::from(a) * u128::from(b)) >> sew_bits;
                    self.set_v_elem(vd, sew_bytes, i, prod as u64);
                }
                0x25 => {
                    // vmul
                    self.set_v_elem(vd, sew_bytes, i, a.wrapping_mul(b));
                }
                0x26 => {
                    // vmulhsu
                    let prod = (i128::from(a_s) * i128::from(b)) >> sew_bits;
                    self.set_v_elem(vd, sew_bytes, i, prod as u64);
                }
                0x27 => {
                    // vmulh
                    let prod = (i128::from(a_s) * i128::from(b_s)) >> sew_bits;
                    self.set_v_elem(vd, sew_bytes, i, prod as u64);
                }
                0x29 => {
                    // vmadd: vd[i] = (b * vd[i]) + a
                    let cur = get_elem(vd, i, sew_bytes);
                    self.set_v_elem(vd, sew_bytes, i, b.wrapping_mul(cur).wrapping_add(a));
                }
                0x2b => {
                    // vnmsub: vd[i] = -(b * vd[i]) + a
                    let cur = get_elem(vd, i, sew_bytes);
                    self.set_v_elem(vd, sew_bytes, i, a.wrapping_sub(b.wrapping_mul(cur)));
                }
                0x2d => {
                    // vmacc: vd[i] = (b * a) + vd[i]
                    let cur = get_elem(vd, i, sew_bytes);
                    self.set_v_elem(vd, sew_bytes, i, b.wrapping_mul(a).wrapping_add(cur));
                }
                0x2f => {
                    // vnmsac: vd[i] = -(b * a) + vd[i]
                    let cur = get_elem(vd, i, sew_bytes);
                    self.set_v_elem(vd, sew_bytes, i, cur.wrapping_sub(b.wrapping_mul(a)));
                }
                0x30..=0x3f => {
                    if sew_bytes >= 8 {
                        return Err(illegal());
                    }
                    let wide_bytes = sew_bytes * 2;
                    let res: u64 = match funct6 {
                        0x30 => a.wrapping_add(b),  // vwaddu.vv / vwaddu.vx
                        0x31 => (a_s + b_s) as u64, // vwadd.vv / vwadd.vx
                        0x32 => a.wrapping_sub(b),  // vwsubu.vv / vwsubu.vx
                        0x33 => (a_s - b_s) as u64, // vwsub.vv / vwsub.vx
                        0x34 => get_elem(vs2, i, wide_bytes).wrapping_add(b), // vwaddu.wv / vwaddu.wx
                        0x35 => {
                            let w_s = sext_sew(get_elem(vs2, i, wide_bytes), wide_bytes);
                            w_s.wrapping_add(b_s) as u64
                        }
                        0x36 => get_elem(vs2, i, wide_bytes).wrapping_sub(b), // vwsubu.wv / vwsubu.wx
                        0x37 => {
                            let w_s = sext_sew(get_elem(vs2, i, wide_bytes), wide_bytes);
                            w_s.wrapping_sub(b_s) as u64
                        }
                        0x38 => a.wrapping_mul(b), // vwmulu.vv / vwmulu.vx
                        0x3a => (a_s * (b as i64)) as u64, // vwmulsu.vv / vwmulsu.vx
                        0x3b => (a_s * b_s) as u64, // vwmul.vv / vwmul.vx
                        0x3c => {
                            let cur = get_elem(vd, i, wide_bytes);
                            cur.wrapping_add(a.wrapping_mul(b))
                        }
                        0x3d => {
                            let cur = get_elem(vd, i, wide_bytes);
                            cur.wrapping_add((a_s * b_s) as u64)
                        }
                        0x3e => {
                            let cur = get_elem(vd, i, wide_bytes);
                            cur.wrapping_add(((b_s as i128) * (a as i128)) as u64)
                        }
                        0x3f if f3 == 6 => {
                            let cur = get_elem(vd, i, wide_bytes);
                            cur.wrapping_add(((b as i128) * (a_s as i128)) as u64)
                        }
                        _ => return Err(illegal()),
                    };
                    self.set_v_elem(vd, wide_bytes, i, res);
                }
                _ => return Err(illegal()),
            }
        }
        self.csr.mstatus |= status::VS;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn exec_vop_fp(
        &mut self,
        insn: &Insn,
        f3: u32,
        funct6: u32,
        vm: u32,
        vd: usize,
        rs1: usize,
        vs2: usize,
        sew_bytes: usize,
    ) -> Result<(), Exception> {
        let illegal = || Exception::new(cause::ILLEGAL_INSN, u64::from(insn.raw));
        if (self.csr.mstatus & status::FS) == 0 || !matches!(sew_bytes, 2 | 4 | 8) {
            return Err(illegal());
        }
        let Some(rm) = RoundingMode::from_bits(self.csr.frm) else {
            return Err(illegal());
        };
        let vl = self.csr.vl as usize;
        let mut fl = Flags::default();

        // 1. Scalar FP moves: vfmv.f.s (f3 == 1, funct6 == 0x10, rs1 == 0) & vfmv.s.f (f3 == 5, funct6 == 0x10, vs2 == 0).
        if funct6 == 0x10 {
            if f3 == 1 && rs1 == 0 && vm == 1 {
                let raw_elem = self.v_elem(vs2, sew_bytes, 0);
                match sew_bytes {
                    2 => self.set_f16(insn.rd, raw_elem as u16),
                    4 => self.set_f32(insn.rd, raw_elem as u32),
                    _ => self.set_f64(insn.rd, raw_elem),
                }
                return Ok(());
            } else if f3 == 5 && vs2 == 0 && vm == 1 {
                if vl > 0 {
                    let fval = match sew_bytes {
                        2 => u64::from(self.f16(insn.rs1)),
                        4 => u64::from(self.f32(insn.rs1)),
                        _ => self.f[rs1],
                    };
                    self.set_v_elem(vd, sew_bytes, 0, fval);
                }
                self.csr.mstatus |= status::VS;
                return Ok(());
            }
            return Err(illegal());
        }

        // 2. FP reductions: vfredusum.vs (0x01), vfredosum.vs (0x03), vfredmin.vs (0x05), vfredmax.vs (0x07).
        if f3 == 1 && matches!(funct6, 0x01 | 0x03 | 0x05 | 0x07) {
            if vl > 0 {
                let mut acc = self.v_elem(rs1, sew_bytes, 0);
                for i in 0..vl {
                    if self.v_mask_bit(vm, i) {
                        let v = self.v_elem(vs2, sew_bytes, i);
                        acc = match (sew_bytes, funct6) {
                            (2, 0x01 | 0x03) => {
                                u64::from(sf::f16_add(acc as u16, v as u16, rm, &mut fl))
                            }
                            (2, 0x05) => u64::from(sf::f16_min(acc as u16, v as u16, &mut fl)),
                            (2, _) => u64::from(sf::f16_max(acc as u16, v as u16, &mut fl)),
                            (4, 0x01 | 0x03) => {
                                u64::from(sf::f32_add(acc as u32, v as u32, rm, &mut fl))
                            }
                            (4, 0x05) => u64::from(sf::f32_min(acc as u32, v as u32, &mut fl)),
                            (4, _) => u64::from(sf::f32_max(acc as u32, v as u32, &mut fl)),
                            (_, 0x01 | 0x03) => sf::f64_add(acc, v, rm, &mut fl),
                            (_, 0x05) => sf::f64_min(acc, v, &mut fl),
                            (_, _) => sf::f64_max(acc, v, &mut fl),
                        };
                    }
                }
                self.set_v_elem(vd, sew_bytes, 0, acc);
            }
            self.accrue(fl);
            self.csr.mstatus |= status::VS;
            return Ok(());
        }

        let old_v = self.v;
        let get_elem = |reg: usize, idx: usize, width: usize| -> u64 {
            let byte_off = idx * width;
            let r = (reg + (byte_off / 32)) & 31;
            let off = byte_off & 31;
            let b = &old_v[r];
            match width {
                1 => u64::from(b[off]),
                2 => u64::from(u16::from_le_bytes([b[off], b[off + 1]])),
                4 => u64::from(u32::from_le_bytes([
                    b[off],
                    b[off + 1],
                    b[off + 2],
                    b[off + 3],
                ])),
                _ => u64::from_le_bytes([
                    b[off],
                    b[off + 1],
                    b[off + 2],
                    b[off + 3],
                    b[off + 4],
                    b[off + 5],
                    b[off + 6],
                    b[off + 7],
                ]),
            }
        };
        let scalar_f = match sew_bytes {
            2 => u64::from(self.f16(insn.rs1)),
            4 => u64::from(self.f32(insn.rs1)),
            _ => self.f[rs1],
        };

        for i in 0..vl {
            let active = self.v_mask_bit(vm, i);
            let a = get_elem(vs2, i, sew_bytes);
            let b = if f3 == 1 {
                get_elem(rs1, i, sew_bytes)
            } else {
                scalar_f
            };

            if funct6 == 0x17 && f3 == 5 {
                // vfmerge.vfm (vm == 0) / vfmv.v.f (vm == 1, vs2 == 0)
                if vm == 1 {
                    if vs2 != 0 {
                        return Err(illegal());
                    }
                    self.set_v_elem(vd, sew_bytes, i, scalar_f);
                } else {
                    let cond = self.v_reg_mask_bit(0, i);
                    self.set_v_elem(vd, sew_bytes, i, if cond { scalar_f } else { a });
                }
                continue;
            }
            if !active {
                continue;
            }

            match funct6 {
                0x00 => {
                    let r = match sew_bytes {
                        2 => u64::from(sf::f16_add(a as u16, b as u16, rm, &mut fl)),
                        4 => u64::from(sf::f32_add(a as u32, b as u32, rm, &mut fl)),
                        _ => sf::f64_add(a, b, rm, &mut fl),
                    };
                    self.set_v_elem(vd, sew_bytes, i, r);
                }
                0x02 => {
                    let r = match sew_bytes {
                        2 => u64::from(sf::f16_sub(a as u16, b as u16, rm, &mut fl)),
                        4 => u64::from(sf::f32_sub(a as u32, b as u32, rm, &mut fl)),
                        _ => sf::f64_sub(a, b, rm, &mut fl),
                    };
                    self.set_v_elem(vd, sew_bytes, i, r);
                }
                0x04 => {
                    let r = match sew_bytes {
                        2 => u64::from(sf::f16_min(a as u16, b as u16, &mut fl)),
                        4 => u64::from(sf::f32_min(a as u32, b as u32, &mut fl)),
                        _ => sf::f64_min(a, b, &mut fl),
                    };
                    self.set_v_elem(vd, sew_bytes, i, r);
                }
                0x06 => {
                    let r = match sew_bytes {
                        2 => u64::from(sf::f16_max(a as u16, b as u16, &mut fl)),
                        4 => u64::from(sf::f32_max(a as u32, b as u32, &mut fl)),
                        _ => sf::f64_max(a, b, &mut fl),
                    };
                    self.set_v_elem(vd, sew_bytes, i, r);
                }
                0x08..=0x0a => {
                    let sign_bit = match sew_bytes {
                        2 => 1u64 << 15,
                        4 => 1u64 << 31,
                        _ => 1u64 << 63,
                    };
                    let sign = match funct6 {
                        0x08 => b & sign_bit,
                        0x09 => (!b) & sign_bit,
                        _ => (a ^ b) & sign_bit,
                    };
                    self.set_v_elem(vd, sew_bytes, i, (a & !sign_bit) | sign);
                }
                0x0e if f3 == 5 => {
                    let val = if i == 0 {
                        scalar_f
                    } else {
                        get_elem(vs2, i - 1, sew_bytes)
                    };
                    self.set_v_elem(vd, sew_bytes, i, val);
                }
                0x0f if f3 == 5 => {
                    let val = if i + 1 < vl {
                        get_elem(vs2, i + 1, sew_bytes)
                    } else {
                        scalar_f
                    };
                    self.set_v_elem(vd, sew_bytes, i, val);
                }
                0x12 if f3 == 1 => {
                    let conv_rm = if matches!(rs1, 6 | 7) {
                        RoundingMode::TowardZero
                    } else {
                        rm
                    };
                    let r = match (sew_bytes, rs1) {
                        (4, 0 | 6) => u64::from(sf::f32_to_u32(a as u32, conv_rm, &mut fl)),
                        (4, 1 | 7) => sf::f32_to_i32(a as u32, conv_rm, &mut fl) as u32 as u64,
                        (4, 2) => u64::from(sf::u32_to_f32(a as u32, rm, &mut fl)),
                        (4, 3) => u64::from(sf::i32_to_f32(a as i32, rm, &mut fl)),
                        (8, 0 | 6) => sf::f64_to_u64(a, conv_rm, &mut fl),
                        (8, 1 | 7) => sf::f64_to_i64(a, conv_rm, &mut fl) as u64,
                        (8, 2) => sf::u64_to_f64(a, rm, &mut fl),
                        (8, 3) => sf::i64_to_f64(a as i64, rm, &mut fl),
                        _ => return Err(illegal()),
                    };
                    self.set_v_elem(vd, sew_bytes, i, r);
                }
                0x13 if f3 == 1 => {
                    let r = match rs1 {
                        0 => match sew_bytes {
                            2 => u64::from(sf::f16_sqrt(a as u16, rm, &mut fl)),
                            4 => u64::from(sf::f32_sqrt(a as u32, rm, &mut fl)),
                            _ => sf::f64_sqrt(a, rm, &mut fl),
                        },
                        16 => match sew_bytes {
                            2 => sf::f16_classify(a as u16),
                            4 => sf::f32_classify(a as u32),
                            _ => sf::f64_classify(a),
                        },
                        _ => return Err(illegal()),
                    };
                    self.set_v_elem(vd, sew_bytes, i, r);
                }
                0x18..=0x1f => {
                    let bit = match (sew_bytes, funct6) {
                        (2, 0x18) => sf::f16_eq(a as u16, b as u16, &mut fl),
                        (2, 0x19) => sf::f16_le(a as u16, b as u16, &mut fl),
                        (2, 0x1b) => sf::f16_lt(a as u16, b as u16, &mut fl),
                        (2, 0x1c) => !sf::f16_eq(a as u16, b as u16, &mut fl),
                        (2, 0x1d) if f3 == 5 => sf::f16_lt(b as u16, a as u16, &mut fl),
                        (2, 0x1f) if f3 == 5 => sf::f16_le(b as u16, a as u16, &mut fl),
                        (4, 0x18) => sf::f32_eq(a as u32, b as u32, &mut fl),
                        (4, 0x19) => sf::f32_le(a as u32, b as u32, &mut fl),
                        (4, 0x1b) => sf::f32_lt(a as u32, b as u32, &mut fl),
                        (4, 0x1c) => !sf::f32_eq(a as u32, b as u32, &mut fl),
                        (4, 0x1d) if f3 == 5 => sf::f32_lt(b as u32, a as u32, &mut fl),
                        (4, 0x1f) if f3 == 5 => sf::f32_le(b as u32, a as u32, &mut fl),
                        (8, 0x18) => sf::f64_eq(a, b, &mut fl),
                        (8, 0x19) => sf::f64_le(a, b, &mut fl),
                        (8, 0x1b) => sf::f64_lt(a, b, &mut fl),
                        (8, 0x1c) => !sf::f64_eq(a, b, &mut fl),
                        (8, 0x1d) if f3 == 5 => sf::f64_lt(b, a, &mut fl),
                        (8, 0x1f) if f3 == 5 => sf::f64_le(b, a, &mut fl),
                        _ => return Err(illegal()),
                    };
                    self.set_v_reg_mask_bit(vd, i, bit);
                }
                0x20 => {
                    let r = match sew_bytes {
                        2 => u64::from(sf::f16_div(a as u16, b as u16, rm, &mut fl)),
                        4 => u64::from(sf::f32_div(a as u32, b as u32, rm, &mut fl)),
                        _ => sf::f64_div(a, b, rm, &mut fl),
                    };
                    self.set_v_elem(vd, sew_bytes, i, r);
                }
                0x21 if f3 == 5 => {
                    let r = match sew_bytes {
                        2 => u64::from(sf::f16_div(b as u16, a as u16, rm, &mut fl)),
                        4 => u64::from(sf::f32_div(b as u32, a as u32, rm, &mut fl)),
                        _ => sf::f64_div(b, a, rm, &mut fl),
                    };
                    self.set_v_elem(vd, sew_bytes, i, r);
                }
                0x24 => {
                    let r = match sew_bytes {
                        2 => u64::from(sf::f16_mul(a as u16, b as u16, rm, &mut fl)),
                        4 => u64::from(sf::f32_mul(a as u32, b as u32, rm, &mut fl)),
                        _ => sf::f64_mul(a, b, rm, &mut fl),
                    };
                    self.set_v_elem(vd, sew_bytes, i, r);
                }
                0x27 if f3 == 5 => {
                    let r = match sew_bytes {
                        2 => u64::from(sf::f16_sub(b as u16, a as u16, rm, &mut fl)),
                        4 => u64::from(sf::f32_sub(b as u32, a as u32, rm, &mut fl)),
                        _ => sf::f64_sub(b, a, rm, &mut fl),
                    };
                    self.set_v_elem(vd, sew_bytes, i, r);
                }
                0x28..=0x2f => {
                    let cur = get_elem(vd, i, sew_bytes);
                    let sign_bit = match sew_bytes {
                        2 => 1u64 << 15,
                        4 => 1u64 << 31,
                        _ => 1u64 << 63,
                    };
                    let (m1, m2, addend) = match funct6 {
                        0x28 => (b, cur, a),                       // vfmadd: +(b * vd) + a
                        0x29 => (b ^ sign_bit, cur, a ^ sign_bit), // vfnmadd: -(b * vd) - a
                        0x2a => (b, cur, a ^ sign_bit),            // vfmsub: +(b * vd) - a
                        0x2b => (b ^ sign_bit, cur, a),            // vfnmsub: -(b * vd) + a
                        0x2c => (b, a, cur),                       // vfmacc: +(b * a) + vd
                        0x2d => (b ^ sign_bit, a, cur ^ sign_bit), // vfnmacc: -(b * a) - vd
                        0x2e => (b, a, cur ^ sign_bit),            // vfmsac: +(b * a) - vd
                        _ => (b ^ sign_bit, a, cur),               // vfnmsac: -(b * a) + vd
                    };
                    let r = match sew_bytes {
                        2 => u64::from(sf::f16_mul_add(
                            m1 as u16,
                            m2 as u16,
                            addend as u16,
                            rm,
                            &mut fl,
                        )),
                        4 => u64::from(sf::f32_mul_add(
                            m1 as u32,
                            m2 as u32,
                            addend as u32,
                            rm,
                            &mut fl,
                        )),
                        _ => sf::f64_mul_add(m1, m2, addend, rm, &mut fl),
                    };
                    let r = match (sew_bytes, r) {
                        (2, _) if (r as u16 & 0x7c00) == 0x7c00 && (r as u16 & 0x03ff) != 0 => {
                            u64::from(F16_CANONICAL_NAN)
                        }
                        (4, _)
                            if (r as u32 & 0x7f80_0000) == 0x7f80_0000
                                && (r as u32 & 0x007f_ffff) != 0 =>
                        {
                            u64::from(F32_CANONICAL_NAN)
                        }
                        (8, _)
                            if (r & 0x7ff0_0000_0000_0000) == 0x7ff0_0000_0000_0000
                                && (r & 0x000f_ffff_ffff_ffff) != 0 =>
                        {
                            F64_CANONICAL_NAN
                        }
                        _ => r,
                    };
                    self.set_v_elem(vd, sew_bytes, i, r);
                }
                _ => return Err(illegal()),
            }
        }
        self.accrue(fl);
        self.csr.mstatus |= status::VS;
        Ok(())
    }
}
