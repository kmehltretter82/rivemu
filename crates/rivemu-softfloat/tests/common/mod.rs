//! Shared helpers for the integration tests: a tiny PRNG and generators of
//! interesting floating-point bit patterns.

#![allow(dead_code)]

use rivemu_softfloat::RoundingMode;

/// All rounding modes.
pub(crate) const MODES: [RoundingMode; 5] = [
    RoundingMode::NearestEven,
    RoundingMode::TowardZero,
    RoundingMode::Down,
    RoundingMode::Up,
    RoundingMode::NearestMaxMagnitude,
];

/// xorshift64* PRNG.
#[derive(Debug)]
pub(crate) struct Rng(u64);

impl Rng {
    pub(crate) fn new(seed: u64) -> Self {
        Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1)
    }

    pub(crate) fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    pub(crate) fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    pub(crate) fn bool(&mut self) -> bool {
        self.next() & 1 != 0
    }

    /// A random rounding mode.
    pub(crate) fn mode(&mut self) -> RoundingMode {
        MODES[self.below(5) as usize]
    }

    /// A mantissa pattern of `bits` bits biased towards edge patterns.
    fn mantissa(&mut self, bits: u32) -> u64 {
        let mask = u64::MAX >> (64 - bits);
        match self.below(8) {
            0 => 0,
            1 => mask,
            2 => 1,
            3 => mask - 1,
            4 => 1 << self.below(bits as u64),
            5 => mask >> self.below(bits as u64),
            6 => (mask << self.below(bits as u64)) & mask,
            _ => self.next() & mask,
        }
    }

    fn generic(&mut self, exp_bits: u32, frac_bits: u32) -> u64 {
        let emax = (1u64 << exp_bits) - 1;
        let bias = emax >> 1;
        let sign = (self.next() & 1) << (exp_bits + frac_bits);
        let exp = match self.below(10) {
            0 => 0,
            1 => emax,
            2 => self.below(4),
            3 => emax - 1 - self.below(4),
            4 | 5 => bias - 30 + self.below(60),
            6 => bias - frac_bits as u64 - 3 + self.below(2 * frac_bits as u64 + 6),
            _ => self.below(emax + 1),
        };
        let frac = self.mantissa(frac_bits);
        sign | (exp << frac_bits) | frac
    }

    /// An interesting binary32 bit pattern (including NaNs, infinities,
    /// zeros and subnormals).
    pub(crate) fn f32(&mut self) -> u32 {
        if self.below(4) == 0 {
            self.next() as u32
        } else {
            self.generic(8, 23) as u32
        }
    }

    /// An interesting binary64 bit pattern.
    pub(crate) fn f64(&mut self) -> u64 {
        if self.below(4) == 0 {
            self.next()
        } else {
            self.generic(11, 52)
        }
    }

    /// A binary32 value close in magnitude to `a` (to provoke cancellation).
    pub(crate) fn f32_near(&mut self, a: u32) -> u32 {
        let delta = (self.below(1 << 26) as i32 - (1 << 25)) >> self.below(26);
        let shifted = (a & 0x7fff_ffff).wrapping_add(delta as u32);
        (shifted & 0x7fff_ffff) | ((self.next() as u32) << 31)
    }

    /// A binary64 value close in magnitude to `a`.
    pub(crate) fn f64_near(&mut self, a: u64) -> u64 {
        let delta = (self.below(1 << 55) as i64 - (1 << 54)) >> self.below(55);
        let shifted = (a & !(1 << 63)).wrapping_add(delta as u64);
        (shifted & !(1 << 63)) | (self.next() << 63)
    }

    /// An interesting integer of up to 64 bits.
    pub(crate) fn int(&mut self) -> u64 {
        let bits = self.below(65) as u32;
        let v = if bits == 0 {
            0
        } else {
            self.mantissa(bits) | (1 << (bits - 1))
        };
        if self.below(4) == 0 {
            v.wrapping_neg()
        } else {
            v
        }
    }
}

/// True if the binary32 pattern is a NaN.
pub(crate) fn is_nan32(x: u32) -> bool {
    x & 0x7fff_ffff > 0x7f80_0000
}

/// True if the binary64 pattern is a NaN.
pub(crate) fn is_nan64(x: u64) -> bool {
    x & !(1 << 63) > 0x7ff0_0000_0000_0000
}
