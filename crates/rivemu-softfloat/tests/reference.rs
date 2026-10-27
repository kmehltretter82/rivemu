//! Cross-checks of all rounding modes against an independent reference.
//!
//! For binary32 operations the exact result is represented as a host `f64`
//! `hi` plus the sign of a (tiny) residual, computed with error-free host
//! transformations (exact products, TwoSum, FMA residuals). An independent
//! rounding routine then derives the expected binary32 result and flags for
//! every rounding mode, including overflow and tininess-after-rounding.
//!
//! For binary64 operations (in the normal range) the host RNE result plus the
//! residual sign determines the directed-rounding results.

mod common;

use common::{MODES, Rng};
use rivemu_softfloat::*;
use std::cmp::Ordering;

const N: usize = 1_000_000;

/// The exact value `hi + d` where `d` is infinitesimal with sign `lo`.
#[derive(Clone, Copy, Debug)]
struct Exact {
    hi: f64,
    lo: Ordering,
}

impl Exact {
    fn cmp(&self, m: f64) -> Ordering {
        match self.hi.partial_cmp(&m).unwrap() {
            Ordering::Equal => self.lo,
            o => o,
        }
    }
    fn neg(self) -> Self {
        Exact {
            hi: -self.hi,
            lo: self.lo.reverse(),
        }
    }
}

fn sign_of(x: f64) -> Ordering {
    x.partial_cmp(&0.0).unwrap()
}

const F32_INF: u32 = 0x7f80_0000;
const TWO_128: f64 = 3.402_823_669_209_385e38; // 2^128

/// Value of a non-negative binary32 pattern on the grid extended past the
/// largest finite value (the "infinity" pattern stands for 2^128).
fn grid(k: u32) -> f64 {
    if k == F32_INF {
        TWO_128
    } else {
        f32::from_bits(k) as f64
    }
}

/// Picks between the lower and upper candidate (magnitudes) given the
/// comparison of the exact magnitude with their midpoint.
fn pick(rm: RoundingMode, neg: bool, cm: Ordering, lower_even: bool) -> bool {
    match rm {
        RoundingMode::NearestEven => match cm {
            Ordering::Less => false,
            Ordering::Greater => true,
            Ordering::Equal => !lower_even,
        },
        RoundingMode::NearestMaxMagnitude => cm != Ordering::Less,
        RoundingMode::TowardZero => false,
        RoundingMode::Up => !neg,
        RoundingMode::Down => neg,
    }
}

/// Reference rounding of a non-zero exact value to binary32.
fn ref_round_f32(x: Exact, rm: RoundingMode) -> (u32, u8) {
    assert!(x.hi != 0.0);
    let neg = x.hi < 0.0;
    let v = if neg { x.neg() } else { x };
    let sign = if neg { 1u32 << 31 } else { 0 };

    if v.cmp(TWO_128) != Ordering::Less {
        let to_inf = pick(rm, neg, Ordering::Greater, false);
        let r = if to_inf { F32_INF } else { F32_INF - 1 };
        return (sign | r, Flags::OVERFLOW | Flags::INEXACT);
    }
    let k = (v.hi as f32).to_bits();
    let (lower, upper) = match v.cmp(grid(k)) {
        Ordering::Equal => return (sign | k, 0),
        Ordering::Greater => (k, k + 1),
        Ordering::Less => (k - 1, k),
    };
    let mid = (grid(lower) + grid(upper)) / 2.0;
    let r = if pick(rm, neg, v.cmp(mid), lower & 1 == 0) {
        upper
    } else {
        lower
    };
    let mut flags = Flags::INEXACT;
    if r == F32_INF {
        flags |= Flags::OVERFLOW;
    }
    let min_normal = f32::MIN_POSITIVE as f64;
    if v.cmp(min_normal) == Ordering::Less {
        // Round with unbounded exponent range at 24-bit precision.
        let lower2 = min_normal - min_normal * f64::powi(2.0, -24);
        let mid2 = min_normal - min_normal * f64::powi(2.0, -25);
        let tiny = v.cmp(lower2) != Ordering::Greater || !pick(rm, neg, v.cmp(mid2), false);
        if tiny {
            flags |= Flags::UNDERFLOW;
        }
    }
    (sign | r, flags)
}

fn finite_nonzero_f32(rng: &mut Rng) -> u32 {
    loop {
        let x = rng.f32();
        if x & 0x7f80_0000 != 0x7f80_0000 && x & 0x7fff_ffff != 0 {
            return x;
        }
    }
}

/// TwoSum: exact `a + b` as `hi + lo`.
fn two_sum(a: f64, b: f64) -> (f64, f64) {
    let s = a + b;
    let bb = s - a;
    (s, (a - (s - bb)) + (b - bb))
}

fn check_f32(op: &str, inputs: &[u32], x: Exact, rm: RoundingMode, got: (u32, u8)) {
    let expected = ref_round_f32(x, rm);
    assert_eq!(
        got, expected,
        "{op}({inputs:08x?}, {rm:?}): got {got:x?}, expected {expected:x?} (exact {x:?})"
    );
}

fn run<T>(f: impl FnOnce(&mut Flags) -> T) -> (T, u8) {
    let mut fl = Flags::default();
    let r = f(&mut fl);
    (r, fl.0)
}

#[test]
fn f32_all_modes() {
    let mut rng = Rng::new(2024);
    for _ in 0..N {
        let a = finite_nonzero_f32(&mut rng);
        let b = if rng.below(4) == 0 {
            let n = rng.f32_near(a);
            if n & 0x7f80_0000 == 0x7f80_0000 || n & 0x7fff_ffff == 0 {
                finite_nonzero_f32(&mut rng)
            } else {
                n
            }
        } else {
            finite_nonzero_f32(&mut rng)
        };
        let c = finite_nonzero_f32(&mut rng);
        let (fa, fb) = (f32::from_bits(a) as f64, f32::from_bits(b) as f64);
        for rm in MODES {
            // mul: exact in f64.
            let x = Exact {
                hi: fa * fb,
                lo: Ordering::Equal,
            };
            check_f32("mul", &[a, b], x, rm, run(|fl| f32_mul(a, b, rm, fl)));

            // add/sub via TwoSum.
            for (op, b2, fb2) in [("add", b, fb), ("sub", b ^ (1 << 31), -fb)] {
                let (hi, lo) = two_sum(fa, fb2);
                let got = run(|fl| {
                    if op == "add" {
                        f32_add(a, b, rm, fl)
                    } else {
                        f32_sub(a, b, rm, fl)
                    }
                });
                if hi == 0.0 {
                    let z = if rm == RoundingMode::Down { 1 << 31 } else { 0 };
                    assert_eq!(got, (z, 0), "{op}({a:08x}, {b2:08x}) exact zero");
                } else {
                    let x = Exact {
                        hi,
                        lo: sign_of(lo),
                    };
                    check_f32(op, &[a, b], x, rm, got);
                }
            }

            // div: residual sign via FMA.
            let q = fa / fb;
            let r = (-q).mul_add(fb, fa);
            let x = Exact {
                hi: q,
                lo: if fb < 0.0 {
                    sign_of(r).reverse()
                } else {
                    sign_of(r)
                },
            };
            check_f32("div", &[a, b], x, rm, run(|fl| f32_div(a, b, rm, fl)));

            // sqrt of |a|.
            let pa = a & 0x7fff_ffff;
            let s = fa.abs().sqrt();
            let r = (-s).mul_add(s, fa.abs());
            let x = Exact {
                hi: s,
                lo: sign_of(r),
            };
            check_f32("sqrt", &[pa], x, rm, run(|fl| f32_sqrt(pa, rm, fl)));

            // fma: exact product plus TwoSum.
            let c2 = if rng.below(3) == 0 {
                // Addend close to -a*b to provoke cancellation.
                let p = f32_mul(a, b, rng.mode(), &mut Flags::default()) ^ (1 << 31);
                if p & 0x7f80_0000 == 0x7f80_0000 || p & 0x7fff_ffff == 0 {
                    c
                } else {
                    p
                }
            } else {
                c
            };
            let fc2 = f32::from_bits(c2) as f64;
            let (hi, lo) = two_sum(fa * fb, fc2);
            let got = run(|fl| f32_mul_add(a, b, c2, rm, fl));
            if hi == 0.0 {
                let z = if rm == RoundingMode::Down { 1 << 31 } else { 0 };
                assert_eq!(got, (z, 0), "fma({a:08x}, {b:08x}, {c2:08x}) exact zero");
            } else {
                let x = Exact {
                    hi,
                    lo: sign_of(lo),
                };
                check_f32("fma", &[a, b, c2], x, rm, got);
            }
        }
    }
}

#[test]
fn f64_to_f32_all_modes() {
    let mut rng = Rng::new(77);
    for _ in 0..N {
        let b = loop {
            let b = if rng.bool() {
                rng.f64()
            } else {
                let e = 1023 - 160 + rng.below(320);
                (rng.next() & 0x800f_ffff_ffff_ffff) | (e << 52)
            };
            if b & 0x7ff0_0000_0000_0000 != 0x7ff0_0000_0000_0000 && b << 1 != 0 {
                break b;
            }
        };
        for rm in MODES {
            let x = Exact {
                hi: f64::from_bits(b),
                lo: Ordering::Equal,
            };
            check_f32(
                "f64_to_f32",
                &[(b >> 32) as u32, b as u32],
                x,
                rm,
                run(|fl| f64_to_f32(b, rm, fl)),
            );
        }
    }
}

#[test]
fn int_to_float_all_modes() {
    let mut rng = Rng::new(5);
    for _ in 0..N {
        let v = rng.int();
        for rm in MODES {
            for (signed, val) in [(true, v as i64 as i128), (false, v as i128)] {
                if val == 0 {
                    continue;
                }
                let hi = val as f64;
                let lo = val.cmp(&(hi as i128));
                let x = Exact { hi, lo };
                let got = run(|fl| {
                    if signed {
                        i64_to_f32(v as i64, rm, fl)
                    } else {
                        u64_to_f32(v, rm, fl)
                    }
                });
                check_f32("int_to_f32", &[v as u32], x, rm, got);

                // binary64 result: expected from the residual direction.
                let got = run(|fl| {
                    if signed {
                        i64_to_f64(v as i64, rm, fl)
                    } else {
                        u64_to_f64(v, rm, fl)
                    }
                });
                let other = if lo == Ordering::Greater {
                    next_up(hi)
                } else {
                    next_down(hi)
                };
                let gap = (other as i128 - hi as i128).abs();
                let tie = 2 * (val - hi as i128).abs() == gap;
                let expected = directed_f64(hi, lo, tie, rm);
                assert_eq!(got, expected, "int_to_f64({val}, {rm:?})");
            }
        }
    }
}

fn next_up(x: f64) -> f64 {
    let b = x.to_bits();
    if x == 0.0 {
        f64::from_bits(1)
    } else if x > 0.0 {
        f64::from_bits(b + 1)
    } else {
        f64::from_bits(b - 1)
    }
}

fn next_down(x: f64) -> f64 {
    -next_up(-x)
}

/// Expected binary64 result in mode `rm` given the RNE result `hi` and the
/// sign of the exact residual (`exact - hi`). `tie` says whether the exact
/// value is a midpoint (only relevant for `NearestMaxMagnitude`).
fn directed_f64(hi: f64, lo: Ordering, tie: bool, rm: RoundingMode) -> (u64, u8) {
    if lo == Ordering::Equal {
        return (hi.to_bits(), 0);
    }
    let other = if lo == Ordering::Greater {
        next_up(hi)
    } else {
        next_down(hi)
    };
    let r = match rm {
        RoundingMode::NearestEven => hi,
        RoundingMode::NearestMaxMagnitude => {
            if tie && other.abs() > hi.abs() {
                other
            } else {
                hi
            }
        }
        RoundingMode::TowardZero => {
            if other.abs() < hi.abs() {
                other
            } else {
                hi
            }
        }
        RoundingMode::Up => hi.max(other),
        RoundingMode::Down => hi.min(other),
    };
    (r.to_bits(), Flags::INEXACT)
}

/// A random binary64 value with exponent in [-400, 400].
fn moderate_f64(rng: &mut Rng) -> u64 {
    let e = 1023 - 400 + rng.below(801);
    let frac = match rng.below(4) {
        0 => (1u64 << 52) - 1 - (rng.next() & 0xff),
        1 => rng.next() & 0xff,
        _ => rng.next(),
    } & ((1 << 52) - 1);
    (rng.next() & (1 << 63)) | (e << 52) | frac
}

#[test]
fn f64_directed_modes() {
    let mut rng = Rng::new(31337);
    for _ in 0..N {
        let a = moderate_f64(&mut rng);
        let b = if rng.below(4) == 0 {
            rng.f64_near(a)
        } else {
            moderate_f64(&mut rng)
        };
        let (fa, fb) = (f64::from_bits(a), f64::from_bits(b));
        if fb == 0.0 || !fb.is_finite() || fb.abs() < 1e-120 || fb.abs() > 1e120 {
            continue;
        }
        let half_gap = |hi: f64, lo: f64| {
            let other = if lo > 0.0 { next_up(hi) } else { next_down(hi) };
            (other - hi).abs() / 2.0
        };
        for rm in MODES {
            // add
            let (hi, lo) = two_sum(fa, fb);
            if hi != 0.0 {
                let tie = lo != 0.0 && lo.abs() == half_gap(hi, lo);
                let expected = directed_f64(hi, sign_of(lo), tie, rm);
                let got = run(|fl| f64_add(a, b, rm, fl));
                assert_eq!(got, expected, "add({a:016x}, {b:016x}, {rm:?})");
            }
            // mul
            let hi = fa * fb;
            let lo = fa.mul_add(fb, -hi);
            let tie = lo != 0.0 && lo.abs() == half_gap(hi, lo);
            let expected = directed_f64(hi, sign_of(lo), tie, rm);
            let got = run(|fl| f64_mul(a, b, rm, fl));
            assert_eq!(got, expected, "mul({a:016x}, {b:016x}, {rm:?})");
            // div (ties are impossible)
            let q = fa / fb;
            let r = (-q).mul_add(fb, fa);
            let lo = if fb < 0.0 {
                sign_of(r).reverse()
            } else {
                sign_of(r)
            };
            let expected = directed_f64(q, lo, false, rm);
            let got = run(|fl| f64_div(a, b, rm, fl));
            assert_eq!(got, expected, "div({a:016x}, {b:016x}, {rm:?})");
            // sqrt (ties are impossible)
            let pa = a & !(1 << 63);
            let s = fa.abs().sqrt();
            let r = (-s).mul_add(s, fa.abs());
            let expected = directed_f64(s, sign_of(r), false, rm);
            let got = run(|fl| f64_sqrt(pa, rm, fl));
            assert_eq!(got, expected, "sqrt({pa:016x}, {rm:?})");
        }
    }
}
