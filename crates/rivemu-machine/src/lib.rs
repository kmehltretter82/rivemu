//! Board models and guest image loaders.
//!
//! Currently provides the RISC-V `virt` board skeleton: RAM, firmware,
//! kernel and initrd placement compatible with QEMU's `riscv64 virt`.

#![forbid(unsafe_code)]

pub mod cpio;
mod fdt;
pub mod loader;
pub mod snapshot;
pub mod virt;

pub use virt::{BootInfo, ImageInfo, Machine, MachineConfig, MachineError, VirtioDeviceConfig};
