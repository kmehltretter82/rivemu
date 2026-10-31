//! Bit-exact IEEE-754 binary32/binary64 arithmetic with RISC-V F/D semantics.
//!
//! All functions operate on raw bit patterns (`u32` for binary32, `u64` for
//! binary64), honour all five RISC-V rounding modes and accumulate the exact
//! IEEE exception flags into a [`Flags`] accumulator (bits are only ever set,
//! never cleared, mirroring the `fflags` CSR).
//!
//! RISC-V specifics implemented here:
//!
//! * every NaN result is the canonical NaN ([`F32_CANONICAL_NAN`] /
//!   [`F64_CANONICAL_NAN`]); payloads are never propagated;
//! * tininess is detected after rounding, and `UNDERFLOW` is only raised when
//!   the tiny result is also inexact;
//! * `fmin`/`fmax` follow the RISC-V F 2.2 (IEEE 754-2019 minimumNumber /
//!   maximumNumber) semantics;
//! * float-to-integer conversions saturate and raise only `INVALID` for NaN
//!   and out-of-range inputs.
//!
//! The implementation uses a single format-generic core with `u128`
//! intermediate significands and one round-and-pack routine; it never
//! allocates.

#![forbid(unsafe_code)]

mod imp;

use imp::{BF16, F16, F32, F64};

/// IEEE rounding mode, numbered like the RISC-V `rm` field / `frm` CSR.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RoundingMode {
    /// Round to nearest, ties to even (`RNE`).
    NearestEven = 0,
    /// Round towards zero (`RTZ`).
    TowardZero = 1,
    /// Round down, towards negative infinity (`RDN`).
    Down = 2,
    /// Round up, towards positive infinity (`RUP`).
    Up = 3,
    /// Round to nearest, ties away from zero (`RMM`).
    NearestMaxMagnitude = 4,
}

impl RoundingMode {
    /// Decodes a RISC-V rounding-mode encoding; returns `None` for the
    /// reserved encodings 5 and 6 and for "dynamic" (7), which the caller
    /// must resolve through `frm` first.
    pub fn from_bits(rm: u64) -> Option<Self> {
        match rm {
            0 => Some(Self::NearestEven),
            1 => Some(Self::TowardZero),
            2 => Some(Self::Down),
            3 => Some(Self::Up),
            4 => Some(Self::NearestMaxMagnitude),
            _ => None,
        }
    }
}

/// Accrued exception flags, in the RISC-V `fflags` layout.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Flags(pub u8);

impl Flags {
    /// Inexact (`NX`).
    pub const INEXACT: u8 = 1;
    /// Underflow (`UF`).
    pub const UNDERFLOW: u8 = 2;
    /// Overflow (`OF`).
    pub const OVERFLOW: u8 = 4;
    /// Division by zero (`DZ`).
    pub const DIV_BY_ZERO: u8 = 8;
    /// Invalid operation (`NV`).
    pub const INVALID: u8 = 16;
}

/// The RISC-V canonical binary16 (`Zfh`) NaN.
pub const F16_CANONICAL_NAN: u16 = 0x7e00;
/// The RISC-V canonical BFloat16 (`Zfbfmin`) NaN.
pub const BF16_CANONICAL_NAN: u16 = 0x7fc0;
/// The RISC-V canonical binary32 NaN.
pub const F32_CANONICAL_NAN: u32 = 0x7fc0_0000;
/// The RISC-V canonical binary64 NaN.
pub const F64_CANONICAL_NAN: u64 = 0x7ff8_0000_0000_0000;

macro_rules! define_format {
    (
        $t:ty, $F:ty, $name:literal,
        $add:ident, $sub:ident, $mul:ident, $div:ident, $sqrt:ident, $fma:ident,
        $min:ident, $max:ident, $minm:ident, $maxm:ident,
        $round:ident, $roundnx:ident,
        $eq:ident, $lt:ident, $le:ident, $ltq:ident, $leq:ident, $classify:ident,
        $to_i32:ident, $to_u32:ident, $to_i64:ident, $to_u64:ident,
        $from_i32:ident, $from_u32:ident, $from_i64:ident, $from_u64:ident $(,)?
    ) => {
        #[doc = concat!("`a + b` in ", $name, ".")]
        #[inline]
        pub fn $add(a: $t, b: $t, rm: RoundingMode, fl: &mut Flags) -> $t {
            imp::add::<$F>(a as u64, b as u64, rm, fl) as $t
        }

        #[doc = concat!("`a - b` in ", $name, ".")]
        #[inline]
        pub fn $sub(a: $t, b: $t, rm: RoundingMode, fl: &mut Flags) -> $t {
            imp::sub::<$F>(a as u64, b as u64, rm, fl) as $t
        }

        #[doc = concat!("`a * b` in ", $name, ".")]
        #[inline]
        pub fn $mul(a: $t, b: $t, rm: RoundingMode, fl: &mut Flags) -> $t {
            imp::mul::<$F>(a as u64, b as u64, rm, fl) as $t
        }

        #[doc = concat!("`a / b` in ", $name, ".")]
        #[inline]
        pub fn $div(a: $t, b: $t, rm: RoundingMode, fl: &mut Flags) -> $t {
            imp::div::<$F>(a as u64, b as u64, rm, fl) as $t
        }

        #[doc = concat!("Square root in ", $name, ".")]
        #[inline]
        pub fn $sqrt(a: $t, rm: RoundingMode, fl: &mut Flags) -> $t {
            imp::sqrt::<$F>(a as u64, rm, fl) as $t
        }

        #[doc = concat!("Fused `a * b + c` in ", $name, " with a single rounding.")]
        ///
        /// `INVALID` is raised for `0 * inf` even when `c` is a quiet NaN.
        /// `fmsub`/`fnmsub`/`fnmadd` are obtained by flipping input sign bits.
        #[inline]
        pub fn $fma(a: $t, b: $t, c: $t, rm: RoundingMode, fl: &mut Flags) -> $t {
            imp::mul_add::<$F>(a as u64, b as u64, c as u64, rm, fl) as $t
        }

        #[doc = concat!("RISC-V `fmin` for ", $name, ".")]
        ///
        /// `-0.0 < +0.0`; if exactly one operand is NaN the other is returned;
        /// two NaNs give the canonical NaN; signaling NaNs raise `INVALID`.
        #[inline]
        pub fn $min(a: $t, b: $t, fl: &mut Flags) -> $t {
            imp::min_max::<$F>(a as u64, b as u64, fl, false) as $t
        }

        #[doc = concat!("RISC-V `fmax` for ", $name, ".")]
        ///
        /// See the corresponding `min` function for NaN and zero handling.
        #[inline]
        pub fn $max(a: $t, b: $t, fl: &mut Flags) -> $t {
            imp::min_max::<$F>(a as u64, b as u64, fl, true) as $t
        }

        #[doc = concat!("Zfa `fminm` (IEEE 754-2019 `minimum`) for ", $name, ".")]
        #[inline]
        pub fn $minm(a: $t, b: $t, fl: &mut Flags) -> $t {
            imp::min_max_m::<$F>(a as u64, b as u64, fl, false) as $t
        }

        #[doc = concat!("Zfa `fmaxm` (IEEE 754-2019 `maximum`) for ", $name, ".")]
        #[inline]
        pub fn $maxm(a: $t, b: $t, fl: &mut Flags) -> $t {
            imp::min_max_m::<$F>(a as u64, b as u64, fl, true) as $t
        }

        #[doc = concat!("Zfa `fround` (`roundToIntegralTiesToEven` etc., without `NX`) for ", $name, ".")]
        #[inline]
        pub fn $round(a: $t, rm: RoundingMode, fl: &mut Flags) -> $t {
            imp::round_to_int::<$F>(a as u64, rm, false, fl) as $t
        }

        #[doc = concat!("Zfa `froundnx` (`roundToIntegralExact`, raising `NX` when inexact) for ", $name, ".")]
        #[inline]
        pub fn $roundnx(a: $t, rm: RoundingMode, fl: &mut Flags) -> $t {
            imp::round_to_int::<$F>(a as u64, rm, true, fl) as $t
        }

        #[doc = concat!("Quiet equality (`feq`) for ", $name, ".")]
        ///
        /// Only signaling NaNs raise `INVALID`.
        #[inline]
        pub fn $eq(a: $t, b: $t, fl: &mut Flags) -> bool {
            imp::eq::<$F>(a as u64, b as u64, fl)
        }

        #[doc = concat!("Signaling less-than (`flt`) for ", $name, ".")]
        ///
        /// Any NaN operand raises `INVALID` and yields `false`.
        #[inline]
        pub fn $lt(a: $t, b: $t, fl: &mut Flags) -> bool {
            imp::lt::<$F>(a as u64, b as u64, fl)
        }

        #[doc = concat!("Signaling less-or-equal (`fle`) for ", $name, ".")]
        ///
        /// Any NaN operand raises `INVALID` and yields `false`.
        #[inline]
        pub fn $le(a: $t, b: $t, fl: &mut Flags) -> bool {
            imp::le::<$F>(a as u64, b as u64, fl)
        }

        #[doc = concat!("Zfa quiet less-than (`fltq`) for ", $name, ".")]
        #[inline]
        pub fn $ltq(a: $t, b: $t, fl: &mut Flags) -> bool {
            imp::ltq::<$F>(a as u64, b as u64, fl)
        }

        #[doc = concat!("Zfa quiet less-or-equal (`fleq`) for ", $name, ".")]
        #[inline]
        pub fn $leq(a: $t, b: $t, fl: &mut Flags) -> bool {
            imp::leq::<$F>(a as u64, b as u64, fl)
        }

        #[doc = concat!("RISC-V `fclass` 10-bit classification mask for ", $name, ".")]
        #[inline]
        pub fn $classify(a: $t) -> u64 {
            imp::classify::<$F>(a as u64)
        }

        #[doc = concat!("Converts ", $name, " to `i32` (`fcvt.w`), saturating.")]
        #[inline]
        pub fn $to_i32(a: $t, rm: RoundingMode, fl: &mut Flags) -> i32 {
            imp::to_int::<$F>(a as u64, rm, fl, true, 32) as i32
        }

        #[doc = concat!("Converts ", $name, " to `u32` (`fcvt.wu`), saturating.")]
        #[inline]
        pub fn $to_u32(a: $t, rm: RoundingMode, fl: &mut Flags) -> u32 {
            imp::to_int::<$F>(a as u64, rm, fl, false, 32) as u32
        }

        #[doc = concat!("Converts ", $name, " to `i64` (`fcvt.l`), saturating.")]
        #[inline]
        pub fn $to_i64(a: $t, rm: RoundingMode, fl: &mut Flags) -> i64 {
            imp::to_int::<$F>(a as u64, rm, fl, true, 64) as i64
        }

        #[doc = concat!("Converts ", $name, " to `u64` (`fcvt.lu`), saturating.")]
        #[inline]
        pub fn $to_u64(a: $t, rm: RoundingMode, fl: &mut Flags) -> u64 {
            imp::to_int::<$F>(a as u64, rm, fl, false, 64) as u64
        }

        #[doc = concat!("Converts an `i32` to ", $name, ".")]
        #[inline]
        pub fn $from_i32(v: i32, rm: RoundingMode, fl: &mut Flags) -> $t {
            imp::from_int::<$F>(v < 0, v.unsigned_abs() as u64, rm, fl) as $t
        }

        #[doc = concat!("Converts a `u32` to ", $name, ".")]
        #[inline]
        pub fn $from_u32(v: u32, rm: RoundingMode, fl: &mut Flags) -> $t {
            imp::from_int::<$F>(false, v as u64, rm, fl) as $t
        }

        #[doc = concat!("Converts an `i64` to ", $name, ".")]
        #[inline]
        pub fn $from_i64(v: i64, rm: RoundingMode, fl: &mut Flags) -> $t {
            imp::from_int::<$F>(v < 0, v.unsigned_abs(), rm, fl) as $t
        }

        #[doc = concat!("Converts a `u64` to ", $name, ".")]
        #[inline]
        pub fn $from_u64(v: u64, rm: RoundingMode, fl: &mut Flags) -> $t {
            imp::from_int::<$F>(false, v, rm, fl) as $t
        }
    };
}

define_format!(
    u16,
    F16,
    "binary16",
    f16_add,
    f16_sub,
    f16_mul,
    f16_div,
    f16_sqrt,
    f16_mul_add,
    f16_min,
    f16_max,
    f16_minm,
    f16_maxm,
    f16_round,
    f16_roundnx,
    f16_eq,
    f16_lt,
    f16_le,
    f16_ltq,
    f16_leq,
    f16_classify,
    f16_to_i32,
    f16_to_u32,
    f16_to_i64,
    f16_to_u64,
    i32_to_f16,
    u32_to_f16,
    i64_to_f16,
    u64_to_f16,
);

define_format!(
    u32,
    F32,
    "binary32",
    f32_add,
    f32_sub,
    f32_mul,
    f32_div,
    f32_sqrt,
    f32_mul_add,
    f32_min,
    f32_max,
    f32_minm,
    f32_maxm,
    f32_round,
    f32_roundnx,
    f32_eq,
    f32_lt,
    f32_le,
    f32_ltq,
    f32_leq,
    f32_classify,
    f32_to_i32,
    f32_to_u32,
    f32_to_i64,
    f32_to_u64,
    i32_to_f32,
    u32_to_f32,
    i64_to_f32,
    u64_to_f32,
);

define_format!(
    u64,
    F64,
    "binary64",
    f64_add,
    f64_sub,
    f64_mul,
    f64_div,
    f64_sqrt,
    f64_mul_add,
    f64_min,
    f64_max,
    f64_minm,
    f64_maxm,
    f64_round,
    f64_roundnx,
    f64_eq,
    f64_lt,
    f64_le,
    f64_ltq,
    f64_leq,
    f64_classify,
    f64_to_i32,
    f64_to_u32,
    f64_to_i64,
    f64_to_u64,
    i32_to_f64,
    u32_to_f64,
    i64_to_f64,
    u64_to_f64,
);

/// Zfa `fcvtmod.w.d`: converts binary64 to signed 32-bit integer modulo `2^32`
/// using `RTZ` rounding.
#[inline]
pub fn f64_to_i32_mod(a: u64, fl: &mut Flags) -> i32 {
    imp::to_i32_mod_f64(a, fl)
}

/// Zfa `fli.h` constant table (indexed by `rs1` in `0..32`).
pub const FLI_H_TABLE: [u16; 32] = [
    0xbc00, // 0: -1.0
    0x0400, // 1: minimum positive normal (2^-14)
    0x0100, // 2: 2^-16 (subnormal)
    0x0200, // 3: 2^-15 (subnormal)
    0x1c00, // 4: 2^-8
    0x2000, // 5: 2^-7
    0x2c00, // 6: 2^-4
    0x3000, // 7: 2^-3
    0x3400, // 8: 0.25
    0x3500, // 9: 0.3125
    0x3600, // 10: 0.375
    0x3700, // 11: 0.4375
    0x3800, // 12: 0.5
    0x3900, // 13: 0.625
    0x3a00, // 14: 0.75
    0x3b00, // 15: 0.875
    0x3c00, // 16: 1.0
    0x3d00, // 17: 1.25
    0x3e00, // 18: 1.5
    0x3f00, // 19: 1.75
    0x4000, // 20: 2.0
    0x4100, // 21: 2.5
    0x4200, // 22: 3.0
    0x4400, // 23: 4.0
    0x4800, // 24: 8.0
    0x4c00, // 25: 16.0
    0x5800, // 26: 128.0
    0x5c00, // 27: 256.0
    0x7800, // 28: 2^15
    0x7c00, // 29: 2^16 (+inf in binary16)
    0x7c00, // 30: +inf
    0x7e00, // 31: canonical NaN
];

/// Zfa `fli.s` constant table (indexed by `rs1` in `0..32`).
pub const FLI_S_TABLE: [u32; 32] = [
    0xbf80_0000, // 0: -1.0
    0x0080_0000, // 1: minimum positive normal (2^-126)
    0x3780_0000, // 2: 2^-16
    0x3800_0000, // 3: 2^-15
    0x3b80_0000, // 4: 2^-8
    0x3c00_0000, // 5: 2^-7
    0x3d80_0000, // 6: 2^-4
    0x3e00_0000, // 7: 2^-3
    0x3e80_0000, // 8: 0.25
    0x3ea0_0000, // 9: 0.3125
    0x3ec0_0000, // 10: 0.375
    0x3ee0_0000, // 11: 0.4375
    0x3f00_0000, // 12: 0.5
    0x3f20_0000, // 13: 0.625
    0x3f40_0000, // 14: 0.75
    0x3f60_0000, // 15: 0.875
    0x3f80_0000, // 16: 1.0
    0x3fa0_0000, // 17: 1.25
    0x3fc0_0000, // 18: 1.5
    0x3fe0_0000, // 19: 1.75
    0x4000_0000, // 20: 2.0
    0x4020_0000, // 21: 2.5
    0x4040_0000, // 22: 3.0
    0x4080_0000, // 23: 4.0
    0x4100_0000, // 24: 8.0
    0x4180_0000, // 25: 16.0
    0x4300_0000, // 26: 128.0
    0x4380_0000, // 27: 256.0
    0x4700_0000, // 28: 2^15
    0x4780_0000, // 29: 2^16
    0x7f80_0000, // 30: +inf
    0x7fc0_0000, // 31: canonical NaN
];

/// Zfa `fli.d` constant table (indexed by `rs1` in `0..32`).
pub const FLI_D_TABLE: [u64; 32] = [
    0xbff0_0000_0000_0000, // 0: -1.0
    0x0010_0000_0000_0000, // 1: minimum positive normal (2^-1022)
    0x3ef0_0000_0000_0000, // 2: 2^-16
    0x3f00_0000_0000_0000, // 3: 2^-15
    0x3f70_0000_0000_0000, // 4: 2^-8
    0x3f80_0000_0000_0000, // 5: 2^-7
    0x3fb0_0000_0000_0000, // 6: 2^-4
    0x3fc0_0000_0000_0000, // 7: 2^-3
    0x3fd0_0000_0000_0000, // 8: 0.25
    0x3fd4_0000_0000_0000, // 9: 0.3125
    0x3fd8_0000_0000_0000, // 10: 0.375
    0x3fdc_0000_0000_0000, // 11: 0.4375
    0x3fe0_0000_0000_0000, // 12: 0.5
    0x3fe4_0000_0000_0000, // 13: 0.625
    0x3fe8_0000_0000_0000, // 14: 0.75
    0x3fec_0000_0000_0000, // 15: 0.875
    0x3ff0_0000_0000_0000, // 16: 1.0
    0x3ff4_0000_0000_0000, // 17: 1.25
    0x3ff8_0000_0000_0000, // 18: 1.5
    0x3ffc_0000_0000_0000, // 19: 1.75
    0x4000_0000_0000_0000, // 20: 2.0
    0x4004_0000_0000_0000, // 21: 2.5
    0x4008_0000_0000_0000, // 22: 3.0
    0x4010_0000_0000_0000, // 23: 4.0
    0x4020_0000_0000_0000, // 24: 8.0
    0x4030_0000_0000_0000, // 25: 16.0
    0x4060_0000_0000_0000, // 26: 128.0
    0x4070_0000_0000_0000, // 27: 256.0
    0x40e0_0000_0000_0000, // 28: 2^15
    0x40f0_0000_0000_0000, // 29: 2^16
    0x7ff0_0000_0000_0000, // 30: +inf
    0x7ff8_0000_0000_0000, // 31: canonical NaN
];

/// Converts binary16 to binary32 (`fcvt.s.h`); always exact.
#[inline]
pub fn f16_to_f32(a: u16, fl: &mut Flags) -> u32 {
    imp::convert::<F16, F32>(u64::from(a), RoundingMode::NearestEven, fl) as u32
}

/// Converts binary32 to binary16 (`fcvt.h.s`), rounding per `rm`.
#[inline]
pub fn f32_to_f16(a: u32, rm: RoundingMode, fl: &mut Flags) -> u16 {
    imp::convert::<F32, F16>(u64::from(a), rm, fl) as u16
}

/// Converts binary16 to binary64 (`fcvt.d.h`); always exact.
#[inline]
pub fn f16_to_f64(a: u16, fl: &mut Flags) -> u64 {
    imp::convert::<F16, F64>(u64::from(a), RoundingMode::NearestEven, fl)
}

/// Converts binary64 to binary16 (`fcvt.h.d`), rounding per `rm`.
#[inline]
pub fn f64_to_f16(a: u64, rm: RoundingMode, fl: &mut Flags) -> u16 {
    imp::convert::<F64, F16>(a, rm, fl) as u16
}

/// Converts BFloat16 to binary32 (`fcvt.s.bf16`); always exact.
#[inline]
pub fn bf16_to_f32(a: u16, fl: &mut Flags) -> u32 {
    imp::convert::<BF16, F32>(u64::from(a), RoundingMode::NearestEven, fl) as u32
}

/// Converts binary32 to BFloat16 (`fcvt.bf16.s`), rounding per `rm`.
#[inline]
pub fn f32_to_bf16(a: u32, rm: RoundingMode, fl: &mut Flags) -> u16 {
    imp::convert::<F32, BF16>(u64::from(a), rm, fl) as u16
}

/// Converts binary32 to binary64 (`fcvt.d.s`); always exact.
///
/// NaN inputs give the canonical binary64 NaN (`INVALID` for signaling NaNs).
#[inline]
pub fn f32_to_f64(a: u32, fl: &mut Flags) -> u64 {
    imp::convert::<F32, F64>(a as u64, RoundingMode::NearestEven, fl)
}

/// Converts binary64 to binary32 (`fcvt.s.d`), rounding per `rm`.
///
/// NaN inputs give the canonical binary32 NaN (`INVALID` for signaling NaNs).
#[inline]
pub fn f64_to_f32(a: u64, rm: RoundingMode, fl: &mut Flags) -> u32 {
    imp::convert::<F64, F32>(a, rm, fl) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_nans_match_format() {
        use imp::Format;
        assert_eq!(F16::NAN, u64::from(F16_CANONICAL_NAN));
        assert_eq!(BF16::NAN, u64::from(BF16_CANONICAL_NAN));
        assert_eq!(F32::NAN, F32_CANONICAL_NAN as u64);
        assert_eq!(F64::NAN, F64_CANONICAL_NAN);
    }
}
