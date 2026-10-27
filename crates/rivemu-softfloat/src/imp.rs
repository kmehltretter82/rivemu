//! Format-generic implementation.
//!
//! All operations work on raw bit patterns widened to `u64`. A finite,
//! non-zero value is represented during computation as `sig * 2^exp` with an
//! integer significand (`u64`/`u128`) and an unbounded `i32` exponent. Every
//! operation computes either the exact result or a result whose lowest bit is
//! a "sticky" (jam) bit far below the rounding position, and then hands it to
//! the single [`round_pack`] routine which performs IEEE rounding, overflow
//! and underflow handling (tininess detected after rounding, as RISC-V
//! requires).

use crate::{Flags, RoundingMode};

/// Description of an IEEE-754 binary interchange format.
pub(crate) trait Format {
    /// Number of exponent bits.
    const EXP_BITS: u32;
    /// Number of explicitly stored fraction bits.
    const FRAC_BITS: u32;

    /// Exponent bias.
    const BIAS: i32 = (1 << (Self::EXP_BITS - 1)) - 1;
    /// All-ones biased exponent (infinities and NaNs).
    const EXP_MAX: u64 = (1 << Self::EXP_BITS) - 1;
    /// Sign bit.
    const SIGN: u64 = 1 << (Self::EXP_BITS + Self::FRAC_BITS);
    /// Fraction field mask.
    const FRAC_MASK: u64 = (1 << Self::FRAC_BITS) - 1;
    /// Quiet bit of a NaN.
    const QUIET: u64 = 1 << (Self::FRAC_BITS - 1);
    /// Positive infinity.
    const INF: u64 = Self::EXP_MAX << Self::FRAC_BITS;
    /// Canonical quiet NaN.
    const NAN: u64 = Self::INF | Self::QUIET;
    /// Largest finite magnitude.
    const MAX_FINITE: u64 = Self::INF - 1;
    /// Unbiased exponent of the smallest normal number.
    const EMIN: i32 = 1 - Self::BIAS;
}

/// IEEE binary32.
#[derive(Debug)]
pub(crate) struct F32;
impl Format for F32 {
    const EXP_BITS: u32 = 8;
    const FRAC_BITS: u32 = 23;
}

/// IEEE binary64.
#[derive(Debug)]
pub(crate) struct F64;
impl Format for F64 {
    const EXP_BITS: u32 = 11;
    const FRAC_BITS: u32 = 52;
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Zero,
    Finite,
    Inf,
    QNaN,
    SNaN,
}

#[derive(Clone, Copy, Debug)]
struct Unpacked {
    sign: bool,
    kind: Kind,
    /// Exponent of the significand's LSB (finite values only).
    exp: i32,
    /// Integer significand (finite values only, non-zero).
    sig: u64,
}

impl Unpacked {
    #[inline]
    fn is_nan(&self) -> bool {
        matches!(self.kind, Kind::QNaN | Kind::SNaN)
    }
    #[inline]
    fn is_snan(&self) -> bool {
        self.kind == Kind::SNaN
    }
}

#[inline]
fn unpack<F: Format>(x: u64) -> Unpacked {
    let sign = x & F::SIGN != 0;
    let be = (x >> F::FRAC_BITS) & F::EXP_MAX;
    let frac = x & F::FRAC_MASK;
    let (kind, exp, sig) = if be == F::EXP_MAX {
        let kind = if frac == 0 {
            Kind::Inf
        } else if frac & F::QUIET != 0 {
            Kind::QNaN
        } else {
            Kind::SNaN
        };
        (kind, 0, 0)
    } else if be == 0 {
        if frac == 0 {
            (Kind::Zero, 0, 0)
        } else {
            (Kind::Finite, F::EMIN - F::FRAC_BITS as i32, frac)
        }
    } else {
        (
            Kind::Finite,
            be as i32 - F::BIAS - F::FRAC_BITS as i32,
            frac | (1 << F::FRAC_BITS),
        )
    };
    Unpacked {
        sign,
        kind,
        exp,
        sig,
    }
}

#[inline]
fn sign_bit<F: Format>(sign: bool) -> u64 {
    if sign { F::SIGN } else { 0 }
}

#[inline]
fn invalid<F: Format>(fl: &mut Flags) -> u64 {
    fl.0 |= Flags::INVALID;
    F::NAN
}

/// Canonical NaN result for an operation with at least one NaN operand.
#[inline]
fn propagate_nan<F: Format>(any_snan: bool, fl: &mut Flags) -> u64 {
    if any_snan {
        fl.0 |= Flags::INVALID;
    }
    F::NAN
}

/// Sign of an exact zero sum of operands with opposite signs.
#[inline]
fn exact_zero<F: Format>(rm: RoundingMode) -> u64 {
    sign_bit::<F>(rm == RoundingMode::Down)
}

/// Shifts `sig` right by `shift` bits and rounds the result to an integer
/// according to `rm` (the value is negative if `sign`). Returns the rounded
/// magnitude and whether it was inexact. A non-positive shift is an exact
/// left shift.
#[inline]
fn round_shift(sig: u128, shift: i32, sign: bool, rm: RoundingMode) -> (u128, bool) {
    if shift <= 0 {
        return (sig << (-shift) as u32, false);
    }
    let (m, half, low) = if shift >= 129 {
        (0, false, sig != 0)
    } else if shift == 128 {
        (0, sig >> 127 != 0, sig << 1 != 0)
    } else {
        let shift = shift as u32;
        let rem = sig & ((1u128 << shift) - 1);
        let half_bit = 1u128 << (shift - 1);
        (sig >> shift, rem & half_bit != 0, rem & (half_bit - 1) != 0)
    };
    let inexact = half || low;
    let inc = match rm {
        RoundingMode::NearestEven => half && (low || m & 1 != 0),
        RoundingMode::NearestMaxMagnitude => half,
        RoundingMode::TowardZero => false,
        RoundingMode::Down => inexact && sign,
        RoundingMode::Up => inexact && !sign,
    };
    (m + inc as u128, inexact)
}

/// Shift right, OR-ing all shifted-out bits into the result's LSB.
#[inline]
fn shift_right_jam(x: u128, d: u32) -> u128 {
    if d == 0 {
        x
    } else if d >= 128 {
        (x != 0) as u128
    } else {
        (x >> d) | ((x & ((1u128 << d) - 1)) != 0) as u128
    }
}

/// Rounds the non-zero value `(-1)^sign * sig * 2^exp` to format `F`.
///
/// `sig` must either be exact or carry a sticky bit in its LSB that lies at
/// least two bits below the final rounding position.
#[inline]
fn round_pack<F: Format>(sign: bool, exp: i32, sig: u128, rm: RoundingMode, fl: &mut Flags) -> u64 {
    debug_assert!(sig != 0);
    let frac = F::FRAC_BITS as i32;
    let n = 128 - sig.leading_zeros() as i32;
    let msb_e = exp + n - 1;
    let mut q = msb_e.max(F::EMIN) - frac;
    let (mut m, inexact) = round_shift(sig, q - exp, sign, rm);
    if m >> (F::FRAC_BITS + 1) != 0 {
        // Rounded up to the next power of two.
        m >>= 1;
        q += 1;
    }
    if inexact {
        fl.0 |= Flags::INEXACT;
        if msb_e < F::EMIN {
            // Tiny before rounding; RISC-V detects tininess after rounding
            // (with unbounded exponent range), which only differs when the
            // value lies just below the smallest normal number.
            let tiny = if msb_e == F::EMIN - 1 {
                let (m2, _) = round_shift(sig, msb_e - frac - exp, sign, rm);
                m2 >> (F::FRAC_BITS + 1) == 0
            } else {
                true
            };
            if tiny {
                fl.0 |= Flags::UNDERFLOW;
            }
        }
    }
    let m = m as u64;
    if m < (1 << F::FRAC_BITS) {
        // Subnormal (or zero); q == EMIN - FRAC_BITS here.
        return sign_bit::<F>(sign) | m;
    }
    let be = q + frac + F::BIAS;
    if be >= F::EXP_MAX as i32 {
        fl.0 |= Flags::OVERFLOW | Flags::INEXACT;
        let to_inf = match rm {
            RoundingMode::NearestEven | RoundingMode::NearestMaxMagnitude => true,
            RoundingMode::TowardZero => false,
            RoundingMode::Down => sign,
            RoundingMode::Up => !sign,
        };
        return sign_bit::<F>(sign) | if to_inf { F::INF } else { F::MAX_FINITE };
    }
    sign_bit::<F>(sign) | ((be as u64) << F::FRAC_BITS) | (m & F::FRAC_MASK)
}

/// Adds two finite non-zero values (`sig < 2^126`).
#[allow(clippy::too_many_arguments)]
#[inline]
fn add_finite<F: Format>(
    sa: bool,
    ea: i32,
    ma: u128,
    sb: bool,
    eb: i32,
    mb: u128,
    rm: RoundingMode,
    fl: &mut Flags,
) -> u64 {
    // Normalise both significands so that their MSB is bit 125.
    let la = ma.leading_zeros() as i32 - 2;
    let lb = mb.leading_zeros() as i32 - 2;
    let (ma, ea) = (ma << la, ea - la);
    let (mb, eb) = (mb << lb, eb - lb);
    let (sa, ea, ma, sb, eb, mb) = if ea >= eb {
        (sa, ea, ma, sb, eb, mb)
    } else {
        (sb, eb, mb, sa, ea, ma)
    };
    // If the exponents differ by at least two, at most one bit cancels, so
    // the jam bit stays far below the rounding position; otherwise the
    // alignment shift is exact.
    let mb = shift_right_jam(mb, (ea - eb) as u32);
    if sa == sb {
        round_pack::<F>(sa, ea, ma + mb, rm, fl)
    } else if ma > mb {
        round_pack::<F>(sa, ea, ma - mb, rm, fl)
    } else if mb > ma {
        round_pack::<F>(sb, ea, mb - ma, rm, fl)
    } else {
        exact_zero::<F>(rm)
    }
}

pub(crate) fn add<F: Format>(a: u64, b: u64, rm: RoundingMode, fl: &mut Flags) -> u64 {
    let x = unpack::<F>(a);
    let y = unpack::<F>(b);
    if x.is_nan() || y.is_nan() {
        return propagate_nan::<F>(x.is_snan() || y.is_snan(), fl);
    }
    match (x.kind, y.kind) {
        (Kind::Inf, Kind::Inf) => {
            if x.sign != y.sign {
                invalid::<F>(fl)
            } else {
                a
            }
        }
        (Kind::Inf, _) => a,
        (_, Kind::Inf) => b,
        (Kind::Zero, Kind::Zero) => {
            if x.sign == y.sign {
                a
            } else {
                exact_zero::<F>(rm)
            }
        }
        (Kind::Zero, _) => b,
        (_, Kind::Zero) => a,
        _ => add_finite::<F>(
            x.sign,
            x.exp,
            x.sig as u128,
            y.sign,
            y.exp,
            y.sig as u128,
            rm,
            fl,
        ),
    }
}

pub(crate) fn sub<F: Format>(a: u64, b: u64, rm: RoundingMode, fl: &mut Flags) -> u64 {
    add::<F>(a, b ^ F::SIGN, rm, fl)
}

pub(crate) fn mul<F: Format>(a: u64, b: u64, rm: RoundingMode, fl: &mut Flags) -> u64 {
    let x = unpack::<F>(a);
    let y = unpack::<F>(b);
    if x.is_nan() || y.is_nan() {
        return propagate_nan::<F>(x.is_snan() || y.is_snan(), fl);
    }
    let sign = x.sign ^ y.sign;
    match (x.kind, y.kind) {
        (Kind::Inf, Kind::Zero) | (Kind::Zero, Kind::Inf) => invalid::<F>(fl),
        (Kind::Inf, _) | (_, Kind::Inf) => sign_bit::<F>(sign) | F::INF,
        (Kind::Zero, _) | (_, Kind::Zero) => sign_bit::<F>(sign),
        _ => round_pack::<F>(sign, x.exp + y.exp, x.sig as u128 * y.sig as u128, rm, fl),
    }
}

/// Normalises a non-zero significand so that its MSB is at `FRAC_BITS`.
#[inline]
fn normalize<F: Format>(sig: u64, exp: i32) -> (u64, i32) {
    let shift = sig.leading_zeros() as i32 - (63 - F::FRAC_BITS as i32);
    (sig << shift, exp - shift)
}

pub(crate) fn div<F: Format>(a: u64, b: u64, rm: RoundingMode, fl: &mut Flags) -> u64 {
    let x = unpack::<F>(a);
    let y = unpack::<F>(b);
    if x.is_nan() || y.is_nan() {
        return propagate_nan::<F>(x.is_snan() || y.is_snan(), fl);
    }
    let sign = x.sign ^ y.sign;
    match (x.kind, y.kind) {
        (Kind::Inf, Kind::Inf) | (Kind::Zero, Kind::Zero) => invalid::<F>(fl),
        (Kind::Inf, _) => sign_bit::<F>(sign) | F::INF,
        (_, Kind::Inf) | (Kind::Zero, _) => sign_bit::<F>(sign),
        (_, Kind::Zero) => {
            fl.0 |= Flags::DIV_BY_ZERO;
            sign_bit::<F>(sign) | F::INF
        }
        _ => {
            let (sa, ea) = normalize::<F>(x.sig, x.exp);
            let (sb, eb) = normalize::<F>(y.sig, y.exp);
            if F::FRAC_BITS < 32 {
                // num < 2^63 and q >= 2^(shift - 1): well over p + 2 bits.
                let shift = 62 - F::FRAC_BITS;
                let num = sa << shift;
                let q = num / sb;
                let r = num % sb;
                let q = q | (r != 0) as u64;
                round_pack::<F>(sign, ea - eb - shift as i32, q as u128, rm, fl)
            } else {
                let shift = 126 - F::FRAC_BITS;
                let num = (sa as u128) << shift;
                let sb = sb as u128;
                let q = num / sb;
                let r = num % sb;
                let q = q | (r != 0) as u128;
                round_pack::<F>(sign, ea - eb - shift as i32, q, rm, fl)
            }
        }
    }
}

/// Integer square root of a `u128` below `2^126`.
#[inline]
fn isqrt_u128(n: u128) -> u128 {
    let mut r = (n as f64).sqrt() as u128;
    if r > 0 {
        r = (r + n / r) >> 1;
    }
    while r * r > n {
        r -= 1;
    }
    while (r + 1) * (r + 1) <= n {
        r += 1;
    }
    r
}

/// Integer square root of a `u64` below `2^62`.
#[inline]
fn isqrt_u64(n: u64) -> u64 {
    let mut r = (n as f64).sqrt() as u64;
    while r * r > n {
        r -= 1;
    }
    while (r + 1) * (r + 1) <= n {
        r += 1;
    }
    r
}

pub(crate) fn sqrt<F: Format>(a: u64, rm: RoundingMode, fl: &mut Flags) -> u64 {
    let x = unpack::<F>(a);
    if x.is_nan() {
        return propagate_nan::<F>(x.is_snan(), fl);
    }
    match x.kind {
        Kind::Zero => a,
        _ if x.sign => invalid::<F>(fl),
        Kind::Inf => a,
        _ => {
            let msb = 63 - x.sig.leading_zeros() as i32;
            if F::FRAC_BITS < 32 {
                // Radicand MSB at bit 60 or 61: root has 31 bits >= p + 2.
                let mut s = 60 - msb;
                if (x.exp - s) & 1 != 0 {
                    s += 1;
                }
                let n = x.sig << s;
                let r = isqrt_u64(n);
                let r = r | (r * r != n) as u64;
                round_pack::<F>(false, (x.exp - s) >> 1, r as u128, rm, fl)
            } else {
                // Radicand MSB at bit 124 or 125: root has 63 bits >= p + 2.
                let mut s = 124 - msb;
                if (x.exp - s) & 1 != 0 {
                    s += 1;
                }
                let n = (x.sig as u128) << s;
                let r = isqrt_u128(n);
                let r = r | (r * r != n) as u128;
                round_pack::<F>(false, (x.exp - s) >> 1, r, rm, fl)
            }
        }
    }
}

pub(crate) fn mul_add<F: Format>(a: u64, b: u64, c: u64, rm: RoundingMode, fl: &mut Flags) -> u64 {
    let x = unpack::<F>(a);
    let y = unpack::<F>(b);
    let z = unpack::<F>(c);
    let inf_times_zero = matches!(
        (x.kind, y.kind),
        (Kind::Inf, Kind::Zero) | (Kind::Zero, Kind::Inf)
    );
    if x.is_nan() || y.is_nan() || z.is_nan() {
        return propagate_nan::<F>(
            x.is_snan() || y.is_snan() || z.is_snan() || inf_times_zero,
            fl,
        );
    }
    if inf_times_zero {
        return invalid::<F>(fl);
    }
    let ps = x.sign ^ y.sign;
    if x.kind == Kind::Inf || y.kind == Kind::Inf {
        if z.kind == Kind::Inf && z.sign != ps {
            return invalid::<F>(fl);
        }
        return sign_bit::<F>(ps) | F::INF;
    }
    if z.kind == Kind::Inf {
        return c;
    }
    if x.kind == Kind::Zero || y.kind == Kind::Zero {
        if z.kind == Kind::Zero {
            return if ps == z.sign {
                sign_bit::<F>(ps)
            } else {
                exact_zero::<F>(rm)
            };
        }
        return c;
    }
    let pe = x.exp + y.exp;
    let pm = x.sig as u128 * y.sig as u128;
    if z.kind == Kind::Zero {
        return round_pack::<F>(ps, pe, pm, rm, fl);
    }
    add_finite::<F>(ps, pe, pm, z.sign, z.exp, z.sig as u128, rm, fl)
}

/// `a < b` for non-NaN values, ordering -0 below +0.
#[inline]
fn total_lt<F: Format>(a: u64, b: u64) -> bool {
    let sa = a & F::SIGN != 0;
    let sb = b & F::SIGN != 0;
    if sa != sb {
        sa
    } else if sa {
        a > b
    } else {
        a < b
    }
}

pub(crate) fn min_max<F: Format>(a: u64, b: u64, fl: &mut Flags, max: bool) -> u64 {
    let x = unpack::<F>(a);
    let y = unpack::<F>(b);
    if x.is_snan() || y.is_snan() {
        fl.0 |= Flags::INVALID;
    }
    match (x.is_nan(), y.is_nan()) {
        (true, true) => F::NAN,
        (true, false) => b,
        (false, true) => a,
        (false, false) => {
            if total_lt::<F>(a, b) != max {
                a
            } else {
                b
            }
        }
    }
}

#[inline]
fn both_zero<F: Format>(a: u64, b: u64) -> bool {
    (a | b) & !F::SIGN == 0
}

pub(crate) fn eq<F: Format>(a: u64, b: u64, fl: &mut Flags) -> bool {
    let x = unpack::<F>(a);
    let y = unpack::<F>(b);
    if x.is_nan() || y.is_nan() {
        if x.is_snan() || y.is_snan() {
            fl.0 |= Flags::INVALID;
        }
        return false;
    }
    a == b || both_zero::<F>(a, b)
}

pub(crate) fn lt<F: Format>(a: u64, b: u64, fl: &mut Flags) -> bool {
    if unpack::<F>(a).is_nan() || unpack::<F>(b).is_nan() {
        fl.0 |= Flags::INVALID;
        return false;
    }
    !both_zero::<F>(a, b) && total_lt::<F>(a, b)
}

pub(crate) fn le<F: Format>(a: u64, b: u64, fl: &mut Flags) -> bool {
    if unpack::<F>(a).is_nan() || unpack::<F>(b).is_nan() {
        fl.0 |= Flags::INVALID;
        return false;
    }
    a == b || both_zero::<F>(a, b) || total_lt::<F>(a, b)
}

pub(crate) fn classify<F: Format>(a: u64) -> u64 {
    let x = unpack::<F>(a);
    let subnormal = (a >> F::FRAC_BITS) & F::EXP_MAX == 0;
    let bit = match (x.kind, x.sign) {
        (Kind::Inf, true) => 0,
        (Kind::Finite, true) if !subnormal => 1,
        (Kind::Finite, true) => 2,
        (Kind::Zero, true) => 3,
        (Kind::Zero, false) => 4,
        (Kind::Finite, false) if subnormal => 5,
        (Kind::Finite, false) => 6,
        (Kind::Inf, false) => 7,
        (Kind::SNaN, _) => 8,
        (Kind::QNaN, _) => 9,
    };
    1 << bit
}

/// Converts to a `width`-bit (signed or unsigned) integer with RISC-V
/// saturation semantics.
pub(crate) fn to_int<F: Format>(
    a: u64,
    rm: RoundingMode,
    fl: &mut Flags,
    signed: bool,
    width: u32,
) -> i128 {
    let x = unpack::<F>(a);
    let (min, max): (i128, i128) = if signed {
        (-(1 << (width - 1)), (1 << (width - 1)) - 1)
    } else {
        (0, (1 << width) - 1)
    };
    let saturate = |neg: bool, fl: &mut Flags| {
        fl.0 |= Flags::INVALID;
        if neg { min } else { max }
    };
    match x.kind {
        Kind::QNaN | Kind::SNaN => saturate(false, fl),
        Kind::Inf => saturate(x.sign, fl),
        Kind::Zero => 0,
        Kind::Finite => {
            let (m, inexact) = if x.exp >= 0 {
                if x.exp > 64 {
                    return saturate(x.sign, fl);
                }
                ((x.sig as u128) << x.exp, false)
            } else {
                round_shift(x.sig as u128, -x.exp, x.sign, rm)
            };
            // m < 2^118, so it fits in an i128.
            let v = if x.sign { -(m as i128) } else { m as i128 };
            if v < min || v > max {
                return saturate(x.sign, fl);
            }
            if inexact {
                fl.0 |= Flags::INEXACT;
            }
            v
        }
    }
}

/// Converts an integer given as sign and magnitude.
pub(crate) fn from_int<F: Format>(sign: bool, mag: u64, rm: RoundingMode, fl: &mut Flags) -> u64 {
    if mag == 0 {
        0
    } else {
        round_pack::<F>(sign, 0, mag as u128, rm, fl)
    }
}

/// Converts between formats.
pub(crate) fn convert<F: Format, G: Format>(a: u64, rm: RoundingMode, fl: &mut Flags) -> u64 {
    let x = unpack::<F>(a);
    match x.kind {
        Kind::QNaN | Kind::SNaN => propagate_nan::<G>(x.is_snan(), fl),
        Kind::Inf => sign_bit::<G>(x.sign) | G::INF,
        Kind::Zero => sign_bit::<G>(x.sign),
        Kind::Finite => round_pack::<G>(x.sign, x.exp, x.sig as u128, rm, fl),
    }
}
