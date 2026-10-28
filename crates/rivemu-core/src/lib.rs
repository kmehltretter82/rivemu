//! Core types shared by all rivemu crates.
//!
//! This crate is intentionally tiny and dependency-free: it only holds
//! vocabulary types (addresses, access sizes, interrupt lines, clocks) that
//! the memory system, CPU and device crates all need to agree on.

#![forbid(unsafe_code)]

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

/// A guest physical (or, in CPU code, virtual) address.
pub type GuestAddr = u64;

/// Binary size units.
pub mod size {
    /// 1 KiB.
    pub const KIB: u64 = 1 << 10;
    /// 1 MiB.
    pub const MIB: u64 = 1 << 20;
    /// 1 GiB.
    pub const GIB: u64 = 1 << 30;
    /// 1 TiB.
    pub const TIB: u64 = 1 << 40;
}

/// Width of a single memory access.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AccessSize {
    /// 8-bit access.
    Byte = 1,
    /// 16-bit access.
    Half = 2,
    /// 32-bit access.
    Word = 4,
    /// 64-bit access.
    Double = 8,
}

impl AccessSize {
    /// Number of bytes covered by the access.
    #[inline]
    pub const fn bytes(self) -> u64 {
        self as u64
    }

    /// Converts a byte count into an access size.
    #[inline]
    pub const fn from_bytes(n: u64) -> Option<Self> {
        match n {
            1 => Some(Self::Byte),
            2 => Some(Self::Half),
            4 => Some(Self::Word),
            8 => Some(Self::Double),
            _ => None,
        }
    }

    /// Bit mask covering the access width (e.g. `0xffff` for [`AccessSize::Half`]).
    #[inline]
    pub const fn mask(self) -> u64 {
        match self {
            Self::Byte => 0xff,
            Self::Half => 0xffff,
            Self::Word => 0xffff_ffff,
            Self::Double => u64::MAX,
        }
    }
}

impl fmt::Display for AccessSize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} bytes", self.bytes())
    }
}

/// A level-triggered interrupt line from a device to an interrupt controller.
pub trait IrqLine: Send + Sync {
    /// Sets the line level (`true` = asserted).
    fn set_level(&self, level: bool);

    /// Asserts the line.
    fn raise(&self) {
        self.set_level(true);
    }

    /// De-asserts the line.
    fn lower(&self) {
        self.set_level(false);
    }
}

/// Source of the platform timebase (RISC-V `mtime` / `time` CSR), in ticks.
pub trait Timebase: Send + Sync {
    /// Current timebase value.
    fn now(&self) -> u64;
}

/// A timebase that never advances (useful for tests).
#[derive(Clone, Copy, Debug, Default)]
pub struct FrozenTimebase(pub u64);

impl Timebase for FrozenTimebase {
    fn now(&self) -> u64 {
        self.0
    }
}

/// A timebase derived from the host monotonic clock.
#[derive(Clone, Copy, Debug)]
pub struct HostTimebase {
    clock: HostClock,
    freq_hz: u64,
}

impl HostTimebase {
    /// Creates a timebase ticking at `freq_hz`, starting at 0.
    pub fn new(freq_hz: u64) -> Self {
        assert!(
            freq_hz > 0 && 1_000_000_000 % freq_hz == 0,
            "unsupported frequency"
        );
        Self {
            clock: HostClock::new(),
            freq_hz,
        }
    }

    /// Ticks per second.
    pub fn freq_hz(&self) -> u64 {
        self.freq_hz
    }

    /// Converts a tick count into nanoseconds.
    pub fn ticks_to_ns(&self, ticks: u64) -> u64 {
        ticks.saturating_mul(1_000_000_000 / self.freq_hz)
    }
}

impl Timebase for HostTimebase {
    fn now(&self) -> u64 {
        self.clock.now_ns() / (1_000_000_000 / self.freq_hz)
    }
}

/// Deterministic instruction-count timebase (`-icount [shift=N]`).
///
/// Virtual time advances by `1 << shift` nanoseconds per retired instruction,
/// and warps forward to the next timer deadline whenever all harts are idle
/// (`wfi`).
#[derive(Debug)]
pub struct IcountTimebase {
    ns: AtomicU64,
    freq_hz: u64,
    shift: u8,
}

impl IcountTimebase {
    /// Creates an instruction-counted timebase ticking at `freq_hz`.
    pub fn new(freq_hz: u64, shift: u8) -> Self {
        assert!(
            freq_hz > 0 && 1_000_000_000 % freq_hz == 0,
            "unsupported frequency"
        );
        Self {
            ns: AtomicU64::new(0),
            freq_hz,
            shift: shift.min(16),
        }
    }

    /// Advances virtual time by `insns` executed instructions.
    #[inline]
    pub fn advance(&self, insns: u64) {
        self.ns.fetch_add(insns << self.shift, Ordering::Relaxed);
    }

    /// Warps virtual time forward so [`Self::now`] is at least `target_ticks`.
    pub fn warp_to(&self, target_ticks: u64) {
        let ns_per_tick = 1_000_000_000 / self.freq_hz;
        let target_ns = target_ticks.saturating_mul(ns_per_tick);
        let _ = self.ns.fetch_max(target_ns, Ordering::Relaxed);
    }

    /// Converts a tick count into nanoseconds.
    pub fn ticks_to_ns(&self, ticks: u64) -> u64 {
        ticks.saturating_mul(1_000_000_000 / self.freq_hz)
    }
}

impl Timebase for IcountTimebase {
    #[inline]
    fn now(&self) -> u64 {
        self.ns.load(Ordering::Relaxed) / (1_000_000_000 / self.freq_hz)
    }
}

/// Monotonic host clock, used as the default time source for guest timers.
#[derive(Clone, Copy, Debug)]
pub struct HostClock {
    start: Instant,
}

impl HostClock {
    /// Creates a clock whose epoch is "now".
    pub fn new() -> Self {
        Self {
            start: Instant::now(),
        }
    }

    /// Nanoseconds elapsed since the clock was created.
    pub fn now_ns(&self) -> u64 {
        u64::try_from(self.start.elapsed().as_nanos()).unwrap_or(u64::MAX)
    }
}

impl Default for HostClock {
    fn default() -> Self {
        Self::new()
    }
}

/// Rounds `value` up to a multiple of `align` (which must be a power of two).
#[inline]
pub const fn align_up(value: u64, align: u64) -> u64 {
    debug_assert!(align.is_power_of_two());
    (value + align - 1) & !(align - 1)
}

/// Rounds `value` down to a multiple of `align` (which must be a power of two).
#[inline]
pub const fn align_down(value: u64, align: u64) -> u64 {
    debug_assert!(align.is_power_of_two());
    value & !(align - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn access_size_roundtrip() {
        for s in [
            AccessSize::Byte,
            AccessSize::Half,
            AccessSize::Word,
            AccessSize::Double,
        ] {
            assert_eq!(AccessSize::from_bytes(s.bytes()), Some(s));
        }
        assert_eq!(AccessSize::from_bytes(3), None);
        assert_eq!(AccessSize::Half.mask(), 0xffff);
    }

    #[test]
    fn alignment() {
        assert_eq!(align_up(0x8000_0001, 0x1000), 0x8000_1000);
        assert_eq!(align_up(0x8000_0000, 0x1000), 0x8000_0000);
        assert_eq!(align_down(0x8000_0fff, 0x1000), 0x8000_0000);
    }

    #[test]
    fn clock_is_monotonic() {
        let c = HostClock::new();
        let a = c.now_ns();
        let b = c.now_ns();
        assert!(b >= a);
    }
}
