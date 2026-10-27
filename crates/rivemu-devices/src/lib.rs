//! Platform devices of the RISC-V `virt` board.

#![forbid(unsafe_code)]

mod clint;
mod plic;
mod power;
mod rtc;
mod uart;

pub use clint::Clint;
pub use plic::{Plic, PlicLine};
pub use power::{PowerControl, PowerEvent, SifiveTest};
pub use rtc::GoldfishRtc;
pub use uart::Uart16550;

/// Extracts the `size`-byte window at byte offset `off & 7` of a 64-bit register.
#[inline]
pub(crate) fn reg_read(reg: u64, off: u64, size: rivemu_core::AccessSize) -> u64 {
    (reg >> (8 * (off & 7))) & size.mask()
}

/// Merges a `size`-byte write at byte offset `off & 7` into a 64-bit register.
#[inline]
pub(crate) fn reg_write(reg: u64, off: u64, size: rivemu_core::AccessSize, value: u64) -> u64 {
    let shift = 8 * (off & 7);
    let mask = size.mask() << shift;
    (reg & !mask) | ((value << shift) & mask)
}
