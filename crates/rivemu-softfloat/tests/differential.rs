//! Differential tests against the host FPU (round-to-nearest-even only for
//! arithmetic; all modes for float-to-integer conversions).

mod common;

use common::{Rng, is_nan32, is_nan64};
use rivemu_softfloat::*;

const N: usize = 2_000_000;
const RNE: RoundingMode = RoundingMode::NearestEven;

fn check32(op: &str, inputs: &[u32], ours: u32, host: f32) {
    let ok = if host.is_nan() {
        ours == F32_CANONICAL_NAN
    } else {
        ours == host.to_bits()
    };
    assert!(
        ok,
        "{op}({inputs:08x?}): ours {ours:08x}, host {:08x}",
        host.to_bits()
    );
}

fn check64(op: &str, inputs: &[u64], ours: u64, host: f64) {
    let ok = if host.is_nan() {
        ours == F64_CANONICAL_NAN
    } else {
        ours == host.to_bits()
    };
    assert!(
        ok,
        "{op}({inputs:016x?}): ours {ours:016x}, host {:016x}",
        host.to_bits()
    );
}

macro_rules! arith_test {
    ($name:ident, $gen:ident, $near:ident, $check:ident, $hf:ty, $sign:expr,
     $add:ident, $sub:ident, $mul:ident, $div:ident, $sqrt:ident, $fma:ident) => {
        #[test]
        fn $name() {
            let mut rng = Rng::new(stringify!($name).len() as u64 * 7919);
            for _ in 0..N {
                let a = rng.$gen();
                let b = if rng.below(4) == 0 {
                    rng.$near(a)
                } else {
                    rng.$gen()
                };
                let c = match rng.below(4) {
                    0 => rng.$near(a),
                    1 => rng.$near(b),
                    _ => rng.$gen(),
                };
                let (fa, fb, fc) = (
                    <$hf>::from_bits(a),
                    <$hf>::from_bits(b),
                    <$hf>::from_bits(c),
                );
                let mut fl = Flags::default();
                $check("add", &[a, b], $add(a, b, RNE, &mut fl), fa + fb);
                $check("sub", &[a, b], $sub(a, b, RNE, &mut fl), fa - fb);
                $check("mul", &[a, b], $mul(a, b, RNE, &mut fl), fa * fb);
                $check("div", &[a, b], $div(a, b, RNE, &mut fl), fa / fb);
                $check("sqrt", &[a], $sqrt(a, RNE, &mut fl), fa.sqrt());
                $check(
                    "fma",
                    &[a, b, c],
                    $fma(a, b, c, RNE, &mut fl),
                    fa.mul_add(fb, fc),
                );
                // fma with an addend that (nearly) cancels the product.
                let p = $mul(a, b, rng.mode(), &mut fl) ^ $sign;
                let fp = <$hf>::from_bits(p);
                $check(
                    "fma-cancel",
                    &[a, b, p],
                    $fma(a, b, p, RNE, &mut fl),
                    fa.mul_add(fb, fp),
                );
            }
        }
    };
}

arith_test!(
    arith_f32,
    f32,
    f32_near,
    check32,
    f32,
    1 << 31,
    f32_add,
    f32_sub,
    f32_mul,
    f32_div,
    f32_sqrt,
    f32_mul_add
);
arith_test!(
    arith_f64,
    f64,
    f64_near,
    check64,
    f64,
    1 << 63,
    f64_add,
    f64_sub,
    f64_mul,
    f64_div,
    f64_sqrt,
    f64_mul_add
);

macro_rules! cmp_test {
    ($name:ident, $gen:ident, $near:ident, $hf:ty, $isnan:ident, $eq:ident, $lt:ident, $le:ident,
     $min:ident, $max:ident, $classify:ident, $canon:expr, $quiet:expr) => {
        #[test]
        fn $name() {
            let mut rng = Rng::new(99);
            let is_snan = |x| $isnan(x) && x & $quiet == 0;
            for _ in 0..N {
                let a = rng.$gen();
                let b = match rng.below(4) {
                    0 => rng.$near(a),
                    1 => a,
                    _ => rng.$gen(),
                };
                let (fa, fb) = (<$hf>::from_bits(a), <$hf>::from_bits(b));
                let any_nan = $isnan(a) || $isnan(b);
                let any_snan = is_snan(a) || is_snan(b);

                let mut fl = Flags::default();
                assert_eq!($eq(a, b, &mut fl), fa == fb, "eq {a:x} {b:x}");
                assert_eq!(fl.0, if any_snan { Flags::INVALID } else { 0 });
                let mut fl = Flags::default();
                assert_eq!($lt(a, b, &mut fl), fa < fb, "lt {a:x} {b:x}");
                assert_eq!(fl.0, if any_nan { Flags::INVALID } else { 0 });
                let mut fl = Flags::default();
                assert_eq!($le(a, b, &mut fl), fa <= fb, "le {a:x} {b:x}");
                assert_eq!(fl.0, if any_nan { Flags::INVALID } else { 0 });

                let (emin, emax) = if $isnan(a) && $isnan(b) {
                    ($canon, $canon)
                } else if $isnan(a) {
                    (b, b)
                } else if $isnan(b) {
                    (a, a)
                } else if fa == 0.0 && fb == 0.0 {
                    (a | b, a & b)
                } else {
                    (fa.min(fb).to_bits(), fa.max(fb).to_bits())
                };
                let mut fl = Flags::default();
                assert_eq!($min(a, b, &mut fl), emin, "min {a:x} {b:x}");
                assert_eq!($max(a, b, &mut fl), emax, "max {a:x} {b:x}");
                assert_eq!(fl.0, if any_snan { Flags::INVALID } else { 0 });

                let class = $classify(a);
                let expected = if $isnan(a) {
                    if is_snan(a) { 8 } else { 9 }
                } else {
                    let pos_bit = if fa.is_infinite() {
                        7
                    } else if fa.is_normal() {
                        6
                    } else if fa == 0.0 {
                        4
                    } else {
                        5
                    };
                    if fa.is_sign_negative() {
                        7 - pos_bit
                    } else {
                        pos_bit
                    }
                };
                assert_eq!(class, 1 << expected, "classify {a:x}");
            }
        }
    };
}

cmp_test!(
    cmp_f32,
    f32,
    f32_near,
    f32,
    is_nan32,
    f32_eq,
    f32_lt,
    f32_le,
    f32_min,
    f32_max,
    f32_classify,
    F32_CANONICAL_NAN,
    0x0040_0000
);
cmp_test!(
    cmp_f64,
    f64,
    f64_near,
    f64,
    is_nan64,
    f64_eq,
    f64_lt,
    f64_le,
    f64_min,
    f64_max,
    f64_classify,
    F64_CANONICAL_NAN,
    0x0008_0000_0000_0000
);

macro_rules! to_int_test {
    ($name:ident, $gen:ident, $hf:ty, $isnan:ident,
     $( $conv:ident => $it:ty ),* ) => {
        #[test]
        fn $name() {
            let mut rng = Rng::new(1234);
            for i in 0..N {
                let a = if i % 2 == 0 {
                    rng.$gen()
                } else {
                    // Values in integer range, often with a fractional part.
                    let v = (rng.int() as i64 as $hf) / (1u64 << rng.below(4)) as $hf;
                    v.to_bits()
                };
                let fa = <$hf>::from_bits(a);
                $(
                    for (rm, host) in [
                        (RoundingMode::TowardZero, fa.trunc()),
                        (RoundingMode::NearestEven, fa.round_ties_even()),
                        (RoundingMode::NearestMaxMagnitude, fa.round()),
                        (RoundingMode::Down, fa.floor()),
                        (RoundingMode::Up, fa.ceil()),
                    ] {
                        let mut fl = Flags::default();
                        let ours = $conv(a, rm, &mut fl);
                        let in_range = !$isnan(a)
                            && host >= <$it>::MIN as $hf
                            && host <= <$it>::MAX as $hf
                            && (host as i128) <= <$it>::MAX as i128;
                        let expected = if $isnan(a) {
                            (<$it>::MAX, Flags::INVALID)
                        } else if in_range {
                            (host as $it, if host != fa { Flags::INEXACT } else { 0 })
                        } else {
                            (host as $it, Flags::INVALID)
                        };
                        assert_eq!(
                            (ours, fl.0),
                            expected,
                            "{}({a:x}, {rm:?})",
                            stringify!($conv)
                        );
                    }
                )*
            }
        }
    };
}

to_int_test!(to_int_f32, f32, f32, is_nan32,
    f32_to_i32 => i32, f32_to_u32 => u32, f32_to_i64 => i64, f32_to_u64 => u64);
to_int_test!(to_int_f64, f64, f64, is_nan64,
    f64_to_i32 => i32, f64_to_u32 => u32, f64_to_i64 => i64, f64_to_u64 => u64);

#[test]
fn from_int() {
    let mut rng = Rng::new(42);
    for _ in 0..N {
        let v = rng.int();
        macro_rules! check {
            ($conv:ident, $it:ty, $hf:ty) => {{
                let x = v as $it;
                let mut fl = Flags::default();
                let ours = $conv(x, RNE, &mut fl);
                let host = x as $hf;
                assert_eq!(ours, host.to_bits(), "{}({x})", stringify!($conv));
                let exact = host as i128 == x as i128;
                assert_eq!(fl.0, if exact { 0 } else { Flags::INEXACT });
            }};
        }
        check!(i32_to_f32, i32, f32);
        check!(u32_to_f32, u32, f32);
        check!(i64_to_f32, i64, f32);
        check!(u64_to_f32, u64, f32);
        check!(i32_to_f64, i32, f64);
        check!(u32_to_f64, u32, f64);
        check!(i64_to_f64, i64, f64);
        check!(u64_to_f64, u64, f64);
    }
}

#[test]
fn float_conversions() {
    let mut rng = Rng::new(7);
    for _ in 0..N {
        let a = rng.f32();
        let mut fl = Flags::default();
        let ours = f32_to_f64(a, &mut fl);
        check64("f32_to_f64", &[a as u64], ours, f32::from_bits(a) as f64);
        let snan = is_nan32(a) && a & 0x0040_0000 == 0;
        assert_eq!(fl.0, if snan { Flags::INVALID } else { 0 });

        let b = if rng.bool() {
            rng.f64()
        } else {
            // Values around the binary32 range, including its subnormals.
            let e = 1023 - 160 + rng.below(320);
            (rng.next() & 0x800f_ffff_ffff_ffff) | (e << 52)
        };
        let mut fl = Flags::default();
        let ours = f64_to_f32(b, RNE, &mut fl);
        check32("f64_to_f32", &[b as u32], ours, f64::from_bits(b) as f32);
        if is_nan64(b) {
            let snan = b & 0x0008_0000_0000_0000 == 0;
            assert_eq!(fl.0, if snan { Flags::INVALID } else { 0 });
        } else {
            let back = f64::from_bits(f32_to_f64(ours, &mut Flags::default()));
            let inexact = back != f64::from_bits(b);
            assert_eq!(fl.0 & Flags::INEXACT != 0, inexact, "f64_to_f32({b:x})");
        }
    }
}
