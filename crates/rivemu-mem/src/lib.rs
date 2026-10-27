//! Guest memory system: RAM backing, MMIO devices and the physical address space.
//!
//! This is one of the two crates in rivemu that is allowed to use `unsafe`
//! (for host pointers into guest RAM). All unsafe code lives in [`ram`].

mod address_space;
mod ram;

pub use address_space::{AddressSpace, BusError, MapError, Region, RegionKind};
pub use ram::Ram;

use rivemu_core::AccessSize;

/// A memory-mapped device.
///
/// Devices are shared (`Arc`) between the address space, interrupt wiring and
/// backends, so access methods take `&self`; devices use interior mutability.
///
/// Offsets are relative to the start of the device's mapping. Accesses to
/// invalid registers should be logged as guest errors and read as zero /
/// be ignored, matching QEMU's behaviour.
pub trait MmioDevice: Send + Sync {
    /// Reads `size` bytes at `offset`. The result must be zero-extended.
    fn read(&self, offset: u64, size: AccessSize) -> u64;

    /// Writes the low `size` bytes of `value` at `offset`.
    fn write(&self, offset: u64, size: AccessSize, value: u64);
}
