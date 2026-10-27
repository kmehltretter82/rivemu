//! RISC-V RV64GC hart model: decoder, interpreter, CSRs, MMU and traps.
//!
//! Implements RV64IMAFDC with Zicsr/Zifencei, M/S/U privilege levels,
//! Sv39/Sv48 paging (hardware A/D updates) and the Sstc timer extension.

#![forbid(unsafe_code)]

pub mod csr;
pub mod decode;
mod fp;
mod hart;
mod mmu;
mod rvc;

pub use hart::{Exception, Hart, HartIrqs, Priv, cause};
