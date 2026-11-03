//! RISC-V RV64GC hart model: decoder, interpreter, CSRs, MMU and traps.
//!
//! Implements RV64IMAFDC with Zicsr/Zifencei, M/S/U privilege levels,
//! Sv39/Sv48 paging (hardware A/D updates) and the Sstc timer extension,
//! and optionally the H extension (`Hart::set_hypervisor`).

#![forbid(unsafe_code)]

pub mod csr;
pub mod decode;
mod fp;
mod hart;
mod hyp;
pub mod log;
mod mmu;
mod rvc;
mod vector;

pub use hart::{Exception, Hart, HartIrqs, Priv, cause};
pub use hyp::H_SNAPSHOT_WORDS;
pub use log::{LogFlags, Logger};
