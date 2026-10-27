//! Hand-written tests for flags, directed rounding and RISC-V edge cases.

use RoundingMode::*;
use rivemu_softfloat::*;

const NX: u8 = Flags::INEXACT;
const UF: u8 = Flags::UNDERFLOW;
const OF: u8 = Flags::OVERFLOW;
const DZ: u8 = Flags::DIV_BY_ZERO;
const NV: u8 = Flags::INVALID;

const F32_SNAN: u32 = 0x7f80_0001;
const F32_QNAN_PAYLOAD: u32 = 0xffc1_2345;
const F64_SNAN: u64 = 0x7ff0_0000_0000_0001;
const F64_QNAN_PAYLOAD: u64 = 0xfff8_0000_dead_beef;
const F64_INF: u64 = 0x7ff0_0000_0000_0000;
const F64_MAX: u64 = 0x7fef_ffff_ffff_ffff;
const F64_MIN_NORMAL: u64 = 0x0010_0000_0000_0000;
const F32_INF: u32 = 0x7f80_0000;
const F32_MAX: u32 = 0x7f7f_ffff;
const NEG64: u64 = 1 << 63;
const NEG32: u32 = 1 << 31;

fn d(x: f64) -> u64 {
    x.to_bits()
}
fn s(x: f32) -> u32 {
    x.to_bits()
}

fn run<T>(f: impl FnOnce(&mut Flags) -> T) -> (T, u8) {
    let mut fl = Flags::default();
    let r = f(&mut fl);
    (r, fl.0)
}

#[test]
fn rounding_mode_from_bits() {
    assert_eq!(RoundingMode::from_bits(0), Some(NearestEven));
    assert_eq!(RoundingMode::from_bits(1), Some(TowardZero));
    assert_eq!(RoundingMode::from_bits(2), Some(Down));
    assert_eq!(RoundingMode::from_bits(3), Some(Up));
    assert_eq!(RoundingMode::from_bits(4), Some(NearestMaxMagnitude));
    for rm in 5..10 {
        assert_eq!(RoundingMode::from_bits(rm), None);
    }
}

#[test]
fn flags_accumulate() {
    let mut fl = Flags(DZ);
    f64_add(d(1.0), d(1e-30), NearestEven, &mut fl);
    assert_eq!(fl.0, DZ | NX);
}

#[test]
fn tiny_addend_directed_rounding() {
    let one = d(1.0);
    let tiny = d(f64::powi(2.0, -60));
    let next = one + 1;
    for (rm, pos, neg) in [
        (NearestEven, one, one),
        (NearestMaxMagnitude, one, one),
        (TowardZero, one, one),
        (Up, next, one),
        (Down, one, next),
    ] {
        assert_eq!(run(|fl| f64_add(one, tiny, rm, fl)), (pos, NX), "{rm:?}");
        assert_eq!(
            run(|fl| f64_sub(one | NEG64, tiny, rm, fl)),
            (neg | NEG64, NX),
            "{rm:?}"
        );
    }
    // binary32
    let one = s(1.0);
    let tiny = s(f32::powi(2.0, -30));
    assert_eq!(run(|fl| f32_add(one, tiny, Up, fl)), (one + 1, NX));
    assert_eq!(run(|fl| f32_add(one, tiny, Down, fl)), (one, NX));
    assert_eq!(run(|fl| f32_sub(one, tiny, Down, fl)), (s(1.0) - 1, NX));
    assert_eq!(
        run(|fl| f32_sub(one, tiny, TowardZero, fl)),
        (s(1.0) - 1, NX)
    );
    assert_eq!(run(|fl| f32_sub(one, tiny, NearestEven, fl)), (one, NX));
}

#[test]
fn ties() {
    // 1 + 2^-53 is exactly halfway between 1 and 1 + 2^-52.
    let one = d(1.0);
    let half_ulp = d(f64::powi(2.0, -53));
    assert_eq!(run(|fl| f64_add(one, half_ulp, NearestEven, fl)), (one, NX));
    assert_eq!(
        run(|fl| f64_add(one, half_ulp, NearestMaxMagnitude, fl)),
        (one + 1, NX)
    );
    // (1 + 2^-52) + 2^-53: tie, even is upward.
    assert_eq!(
        run(|fl| f64_add(one + 1, half_ulp, NearestEven, fl)),
        (one + 2, NX)
    );
    // Negative tie away from zero.
    assert_eq!(
        run(|fl| f64_sub(one | NEG64, half_ulp, NearestMaxMagnitude, fl)),
        ((one + 1) | NEG64, NX)
    );
}

#[test]
fn overflow_each_mode() {
    for (rm, pos, neg) in [
        (NearestEven, F64_INF, F64_INF),
        (NearestMaxMagnitude, F64_INF, F64_INF),
        (TowardZero, F64_MAX, F64_MAX),
        (Up, F64_INF, F64_MAX),
        (Down, F64_MAX, F64_INF),
    ] {
        assert_eq!(
            run(|fl| f64_mul(F64_MAX, d(2.0), rm, fl)),
            (pos, OF | NX),
            "{rm:?}"
        );
        assert_eq!(
            run(|fl| f64_mul(F64_MAX, d(-2.0), rm, fl)),
            (neg | NEG64, OF | NX),
            "{rm:?}"
        );
        assert_eq!(
            run(|fl| f64_add(F64_MAX, F64_MAX, rm, fl)),
            (pos, OF | NX),
            "{rm:?}"
        );
    }
    // binary32 and conversion overflow.
    assert_eq!(
        run(|fl| f32_mul(F32_MAX, s(1.5), TowardZero, fl)),
        (F32_MAX, OF | NX)
    );
    assert_eq!(
        run(|fl| f64_to_f32(d(1e300), NearestEven, fl)),
        (F32_INF, OF | NX)
    );
    assert_eq!(
        run(|fl| f64_to_f32(d(-1e300), Up, fl)),
        (F32_MAX | NEG32, OF | NX)
    );
    // MAX + half ulp rounds up to overflow; just below it does not.
    let max = f32::MAX as f64;
    let half_ulp = f64::powi(2.0, 103);
    assert_eq!(
        run(|fl| f64_to_f32(d(max + half_ulp), NearestEven, fl)),
        (F32_INF, OF | NX)
    );
    assert_eq!(
        run(|fl| f64_to_f32(d(max + half_ulp * 0.5), NearestEven, fl)),
        (F32_MAX, NX)
    );
    assert_eq!(
        run(|fl| f64_to_f32(d(max + half_ulp * 0.5), Up, fl)),
        (F32_INF, OF | NX)
    );
    // u64::MAX in binary32 with RTZ does not overflow.
    assert_eq!(
        run(|fl| u64_to_f32(u64::MAX, TowardZero, fl)),
        (0x5f7f_ffff, NX)
    );
    assert_eq!(
        run(|fl| u64_to_f32(u64::MAX, NearestEven, fl)),
        (s(18446744073709551616.0), NX)
    );
}

#[test]
fn underflow_tininess_after_rounding() {
    // (2 - 2^-52) * 2^-1023 = 2^-1022 - 2^-1075: exactly representable with
    // unbounded exponent (so tiny after rounding), but halfway between the
    // two nearest subnormal-precision neighbours. RNE rounds to the minimum
    // normal, still signalling underflow.
    let a = 0x3fff_ffff_ffff_ffff;
    let b = 0x0008_0000_0000_0000; // 2^-1023
    assert_eq!(
        run(|fl| f64_mul(a, b, NearestEven, fl)),
        (F64_MIN_NORMAL, UF | NX)
    );
    assert_eq!(
        run(|fl| f64_mul(a, b, TowardZero, fl)),
        (F64_MIN_NORMAL - 1, UF | NX)
    );
    assert_eq!(run(|fl| f64_mul(a, b, Up, fl)), (F64_MIN_NORMAL, UF | NX));

    // 2^-126 - 2^-151: with unbounded exponent RNE rounds to 2^-126 (not
    // tiny), so only INEXACT is raised.
    let min = f32::MIN_POSITIVE as f64;
    let v = d(min - f64::powi(2.0, -151));
    assert_eq!(run(|fl| f64_to_f32(v, NearestEven, fl)), (0x0080_0000, NX));
    assert_eq!(run(|fl| f64_to_f32(v, Up, fl)), (0x0080_0000, NX));
    assert_eq!(
        run(|fl| f64_to_f32(v, TowardZero, fl)),
        (0x007f_ffff, UF | NX)
    );
    assert_eq!(run(|fl| f64_to_f32(v | NEG64, Down, fl)), (0x8080_0000, NX));
    // Slightly below that midpoint: tiny after rounding.
    let v = d(min - f64::powi(2.0, -151) - f64::powi(2.0, -170));
    assert_eq!(
        run(|fl| f64_to_f32(v, NearestEven, fl)),
        (0x0080_0000, UF | NX)
    );
    let v = d(min - f64::powi(2.0, -150) - f64::powi(2.0, -170));
    assert_eq!(
        run(|fl| f64_to_f32(v, NearestEven, fl)),
        (0x007f_ffff, UF | NX)
    );
}

#[test]
fn subnormal_results() {
    let min_sub = 1u64;
    // Exact subnormal results raise nothing.
    assert_eq!(run(|fl| f64_mul(min_sub, d(1.0), NearestEven, fl)), (1, 0));
    assert_eq!(
        run(|fl| f64_sub(F64_MIN_NORMAL, 1, NearestEven, fl)),
        (F64_MIN_NORMAL - 1, 0)
    );
    assert_eq!(run(|fl| f64_mul(3, d(0.5), NearestEven, fl)), (2, UF | NX));
    assert_eq!(run(|fl| f64_mul(1, d(0.5), NearestEven, fl)), (0, UF | NX));
    assert_eq!(run(|fl| f64_mul(1, d(0.5), Up, fl)), (1, UF | NX));
    assert_eq!(
        run(|fl| f64_mul(1 | NEG64, d(0.5), Down, fl)),
        (1 | NEG64, UF | NX)
    );
    assert_eq!(
        run(|fl| f64_mul(1 | NEG64, d(0.5), NearestEven, fl)),
        (NEG64, UF | NX)
    );
    assert_eq!(
        run(|fl| f64_mul(1, d(0.5), NearestMaxMagnitude, fl)),
        (1, UF | NX)
    );
    assert_eq!(run(|fl| f32_div(1, s(4.0), Up, fl)), (1, UF | NX));
    assert_eq!(run(|fl| f32_div(F32_MAX, F32_MAX, Up, fl)), (s(1.0), 0));
    // Huge / tiny overflows, tiny / huge underflows.
    assert_eq!(
        run(|fl| f64_div(F64_MAX, 1, NearestEven, fl)),
        (F64_INF, OF | NX)
    );
    assert_eq!(run(|fl| f64_div(1, F64_MAX, NearestEven, fl)), (0, UF | NX));
    assert_eq!(run(|fl| f64_div(1, F64_MAX, Up, fl)), (1, UF | NX));
    // Subnormal inputs are handled exactly.
    assert_eq!(
        run(|fl| f64_sqrt(4, NearestEven, fl)),
        (d(f64::powi(2.0, -536)), 0)
    );
    assert_eq!(
        run(|fl| f32_mul(0x0040_0000, s(4.0), NearestEven, fl)),
        (0x0100_0000, 0)
    );
}

#[test]
fn exact_zero_signs() {
    let one = d(1.0);
    let mone = one | NEG64;
    for rm in [NearestEven, NearestMaxMagnitude, TowardZero, Up] {
        assert_eq!(run(|fl| f64_add(one, mone, rm, fl)), (0, 0));
        assert_eq!(run(|fl| f64_sub(one, one, rm, fl)), (0, 0));
        assert_eq!(run(|fl| f64_add(0, NEG64, rm, fl)), (0, 0));
        assert_eq!(run(|fl| f64_mul_add(one, mone, one, rm, fl)), (0, 0));
        assert_eq!(run(|fl| f64_mul_add(0, one, NEG64, rm, fl)), (0, 0));
    }
    assert_eq!(run(|fl| f64_add(one, mone, Down, fl)), (NEG64, 0));
    assert_eq!(run(|fl| f64_sub(one, one, Down, fl)), (NEG64, 0));
    assert_eq!(run(|fl| f64_add(0, NEG64, Down, fl)), (NEG64, 0));
    assert_eq!(run(|fl| f64_mul_add(one, mone, one, Down, fl)), (NEG64, 0));
    assert_eq!(run(|fl| f64_mul_add(0, one, NEG64, Down, fl)), (NEG64, 0));
    assert_eq!(run(|fl| f32_add(s(2.0), s(-2.0), Down, fl)), (NEG32, 0));
    // Same-signed zeros keep their sign in every mode.
    assert_eq!(run(|fl| f64_add(NEG64, NEG64, NearestEven, fl)), (NEG64, 0));
    assert_eq!(run(|fl| f64_sub(NEG64, 0, Up, fl)), (NEG64, 0));
    assert_eq!(run(|fl| f64_mul_add(NEG64, one, NEG64, Up, fl)), (NEG64, 0));
    assert_eq!(run(|fl| f64_mul_add(0, 0, 0, Down, fl)), (0, 0));
    // x + 0 = x exactly, including -0 + x.
    assert_eq!(run(|fl| f64_add(NEG64, 5, Down, fl)), (5, 0));
    // Product underflowing to zero still has its sign: fma(tiny, -tiny, +0).
    assert_eq!(
        run(|fl| f64_mul_add(1, 1 | NEG64, 0, NearestEven, fl)),
        (NEG64, UF | NX)
    );
    // mul / div sign of zero.
    assert_eq!(run(|fl| f64_mul(NEG64, one, NearestEven, fl)), (NEG64, 0));
    assert_eq!(run(|fl| f64_div(0, mone, NearestEven, fl)), (NEG64, 0));
}

#[test]
fn nan_handling() {
    let one = d(1.0);
    for (r, f) in [
        (run(|fl| f64_add(F64_SNAN, one, NearestEven, fl)), NV),
        (run(|fl| f64_add(F64_QNAN_PAYLOAD, one, NearestEven, fl)), 0),
        (run(|fl| f64_mul(one, F64_SNAN, NearestEven, fl)), NV),
        (run(|fl| f64_div(F64_QNAN_PAYLOAD, 0, NearestEven, fl)), 0),
        (run(|fl| f64_sqrt(F64_SNAN, NearestEven, fl)), NV),
        (run(|fl| f64_sqrt(F64_QNAN_PAYLOAD, NearestEven, fl)), 0),
        (run(|fl| f64_sqrt(mone(), NearestEven, fl)), NV),
        (run(|fl| f64_sqrt(F64_INF | NEG64, NearestEven, fl)), NV),
        (
            run(|fl| f64_add(F64_INF, F64_INF | NEG64, NearestEven, fl)),
            NV,
        ),
        (run(|fl| f64_sub(F64_INF, F64_INF, NearestEven, fl)), NV),
        (run(|fl| f64_mul(0, F64_INF, NearestEven, fl)), NV),
        (run(|fl| f64_div(0, NEG64, NearestEven, fl)), NV),
        (run(|fl| f64_div(F64_INF, F64_INF, NearestEven, fl)), NV),
        (
            run(|fl| f64_mul_add(F64_INF, 0, F64_QNAN_PAYLOAD, NearestEven, fl)),
            NV,
        ),
        (run(|fl| f64_mul_add(0, F64_INF, one, NearestEven, fl)), NV),
        (
            run(|fl| f64_mul_add(one, one, F64_SNAN, NearestEven, fl)),
            NV,
        ),
        (
            run(|fl| f64_mul_add(one, F64_QNAN_PAYLOAD, one, NearestEven, fl)),
            0,
        ),
        (
            run(|fl| f64_mul_add(F64_INF, one, F64_INF | NEG64, NearestEven, fl)),
            NV,
        ),
    ] {
        assert_eq!(r, (F64_CANONICAL_NAN, f));
    }
    for (r, f) in [
        (run(|fl| f32_add(F32_SNAN, s(1.0), NearestEven, fl)), NV),
        (
            run(|fl| f32_add(F32_QNAN_PAYLOAD, s(1.0), NearestEven, fl)),
            0,
        ),
        (
            run(|fl| f32_mul_add(F32_INF, 0, F32_QNAN_PAYLOAD, NearestEven, fl)),
            NV,
        ),
        (run(|fl| f32_sqrt(s(-0.5), NearestEven, fl)), NV),
        (run(|fl| f64_to_f32(F64_SNAN, NearestEven, fl)), NV),
        (run(|fl| f64_to_f32(F64_QNAN_PAYLOAD, NearestEven, fl)), 0),
    ] {
        assert_eq!(r, (F32_CANONICAL_NAN, f));
    }
    assert_eq!(run(|fl| f32_to_f64(F32_SNAN, fl)), (F64_CANONICAL_NAN, NV));
    assert_eq!(
        run(|fl| f32_to_f64(F32_QNAN_PAYLOAD, fl)),
        (F64_CANONICAL_NAN, 0)
    );
    // Infinity arithmetic that is valid.
    assert_eq!(run(|fl| f64_sqrt(F64_INF, NearestEven, fl)), (F64_INF, 0));
    assert_eq!(run(|fl| f64_sqrt(NEG64, NearestEven, fl)), (NEG64, 0));
    assert_eq!(
        run(|fl| f64_mul_add(F64_INF, mone(), one, NearestEven, fl)),
        (F64_INF | NEG64, 0)
    );
    assert_eq!(
        run(|fl| f64_mul_add(one, one, F64_INF | NEG64, NearestEven, fl)),
        (F64_INF | NEG64, 0)
    );
}

fn mone() -> u64 {
    d(-1.0)
}

#[test]
fn division_by_zero() {
    assert_eq!(run(|fl| f64_div(d(1.0), 0, NearestEven, fl)), (F64_INF, DZ));
    assert_eq!(
        run(|fl| f64_div(d(-1.0), 0, NearestEven, fl)),
        (F64_INF | NEG64, DZ)
    );
    assert_eq!(
        run(|fl| f64_div(d(1.0), NEG64, NearestEven, fl)),
        (F64_INF | NEG64, DZ)
    );
    assert_eq!(run(|fl| f32_div(1, 0, NearestEven, fl)), (F32_INF, DZ));
    // inf / 0 is exact infinity without DZ.
    assert_eq!(run(|fl| f64_div(F64_INF, 0, NearestEven, fl)), (F64_INF, 0));
}

#[test]
fn sqrt_values() {
    assert_eq!(run(|fl| f64_sqrt(d(4.0), NearestEven, fl)), (d(2.0), 0));
    assert_eq!(
        run(|fl| f64_sqrt(d(2.0), NearestEven, fl)),
        (d(std::f64::consts::SQRT_2), NX)
    );
    // The RNE result 0x3ff6a09e667f3bcd lies above the exact root.
    assert_eq!(
        run(|fl| f64_sqrt(d(2.0), Up, fl)),
        (d(std::f64::consts::SQRT_2), NX)
    );
    assert_eq!(
        run(|fl| f64_sqrt(d(2.0), Down, fl)),
        (d(std::f64::consts::SQRT_2) - 1, NX)
    );
    assert_eq!(run(|fl| f32_sqrt(s(2.0), Down, fl)), (0x3fb5_04f3, NX));
    assert_eq!(run(|fl| f32_sqrt(s(2.0), Up, fl)), (0x3fb5_04f4, NX));
}

#[test]
fn fused_single_rounding() {
    // (1 + 2^-52)^2 - (1 + 2^-51) = 2^-104, lost by separate rounding.
    let a = d(1.0) + 1;
    let c = (d(1.0) + 2) | NEG64;
    assert_eq!(
        run(|fl| f64_mul_add(a, a, c, NearestEven, fl)),
        (d(f64::powi(2.0, -104)), 0)
    );
    // Product far below the addend's ulp: directed rounding must see it.
    let tiny = d(f64::powi(2.0, -600));
    assert_eq!(
        run(|fl| f64_mul_add(tiny, tiny, d(1.0), Up, fl)),
        (d(1.0) + 1, NX)
    );
    assert_eq!(
        run(|fl| f64_mul_add(tiny, tiny | NEG64, d(1.0), TowardZero, fl)),
        (d(1.0) - 1, NX)
    );
    assert_eq!(
        run(|fl| f64_mul_add(tiny, tiny | NEG64, d(1.0), NearestEven, fl)),
        (d(1.0), NX)
    );
}

#[test]
fn min_max() {
    let one = d(1.0);
    assert_eq!(run(|fl| f64_min(0, NEG64, fl)), (NEG64, 0));
    assert_eq!(run(|fl| f64_min(NEG64, 0, fl)), (NEG64, 0));
    assert_eq!(run(|fl| f64_max(0, NEG64, fl)), (0, 0));
    assert_eq!(run(|fl| f64_max(NEG64, 0, fl)), (0, 0));
    assert_eq!(run(|fl| f64_min(F64_QNAN_PAYLOAD, one, fl)), (one, 0));
    assert_eq!(run(|fl| f64_max(one, F64_QNAN_PAYLOAD, fl)), (one, 0));
    assert_eq!(run(|fl| f64_min(F64_SNAN, one, fl)), (one, NV));
    assert_eq!(run(|fl| f64_max(one, F64_SNAN, fl)), (one, NV));
    assert_eq!(
        run(|fl| f64_min(F64_QNAN_PAYLOAD, F64_QNAN_PAYLOAD, fl)),
        (F64_CANONICAL_NAN, 0)
    );
    assert_eq!(
        run(|fl| f64_max(F64_SNAN, F64_QNAN_PAYLOAD, fl)),
        (F64_CANONICAL_NAN, NV)
    );
    assert_eq!(run(|fl| f32_min(s(-3.0), s(2.0), fl)), (s(-3.0), 0));
    assert_eq!(run(|fl| f32_max(s(-3.0), s(-2.0), fl)), (s(-2.0), 0));
    assert_eq!(
        run(|fl| f32_max(F32_INF | NEG32, F32_SNAN, fl)),
        (F32_INF | NEG32, NV)
    );
}

#[test]
fn compares() {
    let one = d(1.0);
    assert_eq!(run(|fl| f64_eq(0, NEG64, fl)), (true, 0));
    assert_eq!(run(|fl| f64_lt(NEG64, 0, fl)), (false, 0));
    assert_eq!(run(|fl| f64_le(NEG64, 0, fl)), (true, 0));
    assert_eq!(run(|fl| f64_eq(F64_QNAN_PAYLOAD, one, fl)), (false, 0));
    assert_eq!(run(|fl| f64_eq(F64_SNAN, one, fl)), (false, NV));
    assert_eq!(run(|fl| f64_lt(F64_QNAN_PAYLOAD, one, fl)), (false, NV));
    assert_eq!(run(|fl| f64_le(one, F64_QNAN_PAYLOAD, fl)), (false, NV));
    assert_eq!(run(|fl| f64_lt(mone(), one, fl)), (true, 0));
    assert_eq!(run(|fl| f64_lt(d(-2.0), mone(), fl)), (true, 0));
    assert_eq!(run(|fl| f64_le(F64_INF, F64_INF, fl)), (true, 0));
    assert_eq!(run(|fl| f32_lt(s(1.0), s(2.0), fl)), (true, 0));
    assert_eq!(run(|fl| f32_eq(F32_SNAN, F32_SNAN, fl)), (false, NV));
}

#[test]
fn classify() {
    let cases64 = [
        (F64_INF | NEG64, 0),
        (d(-1.0), 1),
        (1 | NEG64, 2),
        (NEG64, 3),
        (0, 4),
        (1, 5),
        (d(1.0), 6),
        (F64_INF, 7),
        (F64_SNAN, 8),
        (F64_CANONICAL_NAN, 9),
        (F64_QNAN_PAYLOAD, 9),
    ];
    for (x, bit) in cases64 {
        assert_eq!(f64_classify(x), 1 << bit, "{x:x}");
    }
    let cases32 = [
        (F32_INF | NEG32, 0),
        (s(-1.0), 1),
        (0x807f_ffff, 2),
        (NEG32, 3),
        (0, 4),
        (0x007f_ffff, 5),
        (0x0080_0000, 6),
        (F32_INF, 7),
        (F32_SNAN, 8),
        (F32_CANONICAL_NAN, 9),
    ];
    for (x, bit) in cases32 {
        assert_eq!(f32_classify(x), 1 << bit, "{x:x}");
    }
}

#[test]
fn float_to_int_edge_cases() {
    let two31 = 2147483648.0f64;
    assert_eq!(
        run(|fl| f64_to_i32(d(two31 - 0.5), NearestEven, fl)),
        (i32::MAX, NV)
    );
    assert_eq!(
        run(|fl| f64_to_i32(d(two31 - 0.5), TowardZero, fl)),
        (i32::MAX, NX)
    );
    assert_eq!(
        run(|fl| f64_to_i32(d(two31 - 0.5), Down, fl)),
        (i32::MAX, NX)
    );
    assert_eq!(run(|fl| f64_to_i32(d(two31 - 0.5), Up, fl)), (i32::MAX, NV));
    assert_eq!(
        run(|fl| f64_to_i32(d(-two31 - 0.5), NearestEven, fl)),
        (i32::MIN, NX)
    );
    assert_eq!(
        run(|fl| f64_to_i32(d(-two31 - 0.5), NearestMaxMagnitude, fl)),
        (i32::MIN, NV)
    );
    assert_eq!(
        run(|fl| f64_to_i32(d(-two31), NearestEven, fl)),
        (i32::MIN, 0)
    );
    assert_eq!(run(|fl| f64_to_i32(d(2.5), NearestEven, fl)), (2, NX));
    assert_eq!(
        run(|fl| f64_to_i32(d(2.5), NearestMaxMagnitude, fl)),
        (3, NX)
    );
    assert_eq!(
        run(|fl| f64_to_i32(d(-2.5), NearestMaxMagnitude, fl)),
        (-3, NX)
    );
    assert_eq!(run(|fl| f64_to_i32(d(-2.5), Up, fl)), (-2, NX));
    assert_eq!(run(|fl| f64_to_i32(d(-2.5), Down, fl)), (-3, NX));
    assert_eq!(run(|fl| f64_to_i32(d(1e-300), Up, fl)), (1, NX));
    assert_eq!(run(|fl| f64_to_i32(NEG64, Up, fl)), (0, 0));

    // Unsigned: negative values rounding to zero are valid.
    assert_eq!(run(|fl| f64_to_u32(d(-0.3), NearestEven, fl)), (0, NX));
    assert_eq!(run(|fl| f64_to_u32(d(-0.5), NearestEven, fl)), (0, NX));
    assert_eq!(
        run(|fl| f64_to_u32(d(-0.5), NearestMaxMagnitude, fl)),
        (0, NV)
    );
    assert_eq!(run(|fl| f64_to_u32(d(-0.3), Down, fl)), (0, NV));
    assert_eq!(run(|fl| f64_to_u32(d(-0.9), TowardZero, fl)), (0, NX));
    assert_eq!(run(|fl| f64_to_u32(d(-1.0), TowardZero, fl)), (0, NV));
    assert_eq!(run(|fl| f64_to_u32(NEG64, TowardZero, fl)), (0, 0));
    assert_eq!(
        run(|fl| f64_to_u32(d(4294967295.5), TowardZero, fl)),
        (u32::MAX, NX)
    );
    assert_eq!(
        run(|fl| f64_to_u32(d(4294967295.5), NearestEven, fl)),
        (u32::MAX, NV)
    );

    // NaN and infinities.
    assert_eq!(
        run(|fl| f64_to_i32(F64_QNAN_PAYLOAD, TowardZero, fl)),
        (i32::MAX, NV)
    );
    assert_eq!(
        run(|fl| f64_to_u32(F64_QNAN_PAYLOAD, TowardZero, fl)),
        (u32::MAX, NV)
    );
    assert_eq!(
        run(|fl| f64_to_i64(F64_SNAN, TowardZero, fl)),
        (i64::MAX, NV)
    );
    assert_eq!(
        run(|fl| f64_to_u64(F64_CANONICAL_NAN, TowardZero, fl)),
        (u64::MAX, NV)
    );
    assert_eq!(
        run(|fl| f32_to_i32(F32_CANONICAL_NAN, TowardZero, fl)),
        (i32::MAX, NV)
    );
    assert_eq!(
        run(|fl| f32_to_u64(F32_SNAN, TowardZero, fl)),
        (u64::MAX, NV)
    );
    assert_eq!(
        run(|fl| f64_to_i64(F64_INF | NEG64, TowardZero, fl)),
        (i64::MIN, NV)
    );
    assert_eq!(
        run(|fl| f64_to_u64(F64_INF | NEG64, TowardZero, fl)),
        (0, NV)
    );
    assert_eq!(
        run(|fl| f64_to_u64(F64_INF, TowardZero, fl)),
        (u64::MAX, NV)
    );
    assert_eq!(
        run(|fl| f32_to_i64(F32_INF, TowardZero, fl)),
        (i64::MAX, NV)
    );

    // 64-bit boundaries.
    let two63 = 9223372036854775808.0f64;
    assert_eq!(
        run(|fl| f64_to_i64(d(two63), TowardZero, fl)),
        (i64::MAX, NV)
    );
    assert_eq!(
        run(|fl| f64_to_i64(d(-two63), TowardZero, fl)),
        (i64::MIN, 0)
    );
    assert_eq!(
        run(|fl| f64_to_u64(d(two63 * 2.0), TowardZero, fl)),
        (u64::MAX, NV)
    );
    assert_eq!(
        run(|fl| f64_to_u64(d(two63 * 2.0) - 1, TowardZero, fl)),
        (0xffff_ffff_ffff_f800, 0)
    );
    assert_eq!(
        run(|fl| f64_to_i64(d(1e300), TowardZero, fl)),
        (i64::MAX, NV)
    );
    assert_eq!(
        run(|fl| f32_to_i32(s(-2147483648.0), Up, fl)),
        (i32::MIN, 0)
    );
}

#[test]
fn int_to_float_edge_cases() {
    assert_eq!(
        run(|fl| i32_to_f32(16_777_217, NearestEven, fl)),
        (s(16_777_216.0), NX)
    );
    assert_eq!(
        run(|fl| i32_to_f32(16_777_217, Up, fl)),
        (s(16_777_218.0), NX)
    );
    assert_eq!(
        run(|fl| i32_to_f32(-16_777_217, Up, fl)),
        (s(-16_777_216.0), NX)
    );
    assert_eq!(
        run(|fl| i32_to_f32(-16_777_217, Down, fl)),
        (s(-16_777_218.0), NX)
    );
    assert_eq!(
        run(|fl| i32_to_f32(-16_777_217, NearestMaxMagnitude, fl)),
        (s(-16_777_218.0), NX)
    );
    assert_eq!(
        run(|fl| i32_to_f32(i32::MIN, NearestEven, fl)),
        (s(-2147483648.0), 0)
    );
    assert_eq!(
        run(|fl| i64_to_f64(i64::MIN, NearestEven, fl)),
        (d(-9223372036854775808.0), 0)
    );
    assert_eq!(run(|fl| i64_to_f64(0, Down, fl)), (0, 0));
    assert_eq!(
        run(|fl| u32_to_f64(u32::MAX, NearestEven, fl)),
        (d(4294967295.0), 0)
    );
    assert_eq!(run(|fl| i32_to_f64(-7, NearestEven, fl)), (d(-7.0), 0));
    assert_eq!(
        run(|fl| u64_to_f64(u64::MAX, TowardZero, fl)),
        (0x43ef_ffff_ffff_ffff, NX)
    );
}

#[test]
fn format_conversions() {
    assert_eq!(run(|fl| f32_to_f64(1, fl)), (d(f64::powi(2.0, -149)), 0));
    assert_eq!(run(|fl| f32_to_f64(NEG32, fl)), (NEG64, 0));
    assert_eq!(run(|fl| f32_to_f64(F32_INF, fl)), (F64_INF, 0));
    assert_eq!(
        run(|fl| f64_to_f32(d(f64::powi(2.0, -149)), NearestEven, fl)),
        (1, 0)
    );
    assert_eq!(
        run(|fl| f64_to_f32(d(f64::powi(2.0, -150)), NearestEven, fl)),
        (0, UF | NX)
    );
    assert_eq!(
        run(|fl| f64_to_f32(d(f64::powi(2.0, -150)), Up, fl)),
        (1, UF | NX)
    );
    assert_eq!(run(|fl| f64_to_f32(1, Up, fl)), (1, UF | NX));
    assert_eq!(
        run(|fl| f64_to_f32(1 | NEG64, TowardZero, fl)),
        (NEG32, UF | NX)
    );
    assert_eq!(run(|fl| f64_to_f32(d(0.1), NearestEven, fl)), (s(0.1), NX));
    assert_eq!(run(|fl| f64_to_f32(d(0.1), Up, fl)), (s(0.1), NX));
    assert_eq!(run(|fl| f64_to_f32(d(0.1), Down, fl)), (s(0.1) - 1, NX));
}
