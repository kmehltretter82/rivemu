//! Pure-Rust PCIe NVMe 1.4 Controller (`1b36:0010`, Class `0x010802`).
//!
//! Exposes a 16 KiB 32-bit Memory BAR0 containing:
//! - `0x0000..0x0050`: NVMe Controller Registers (`CAP`, `VS`, `INTMS`, `INTMC`, `CC`, `CSTS`, `AQA`, `ASQ`, `ACQ`)
//! - `0x1000..0x1100`: Submission Queue Tail Doorbells (`SQyTDBL`) and Completion Queue Head Doorbells (`CQyHDBL`)
//!
//! Supports Admin Queue (`QID 0`) and up to 15 I/O Queue pairs (`QID 1..=15`)
//! with 4 KiB PRP1 / PRP2 / PRP-list DMA and single-namespace (`NSID 1`)
//! 512-byte LBA Read / Write / Flush operations backed by a [`BlockBackend`].

use std::fmt;
use std::sync::Arc;

use rivemu_backends::{BlockBackend, SECTOR_SIZE};
use rivemu_core::{AccessSize, IrqLine};

use crate::pci::{PCIE_MMIO_BASE, VIRTIO_PCI_BAR0_SIZE};
use crate::queue::DmaRam;

const MAX_QUEUES: usize = 16;
const PAGE_SIZE: u64 = 4096;

/// State of an NVMe Submission Queue (`SQ`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NvmeSq {
    /// Base guest physical address (`PRP1` / `ASQ`).
    pub base: u64,
    /// Number of entries (`1..=64`).
    pub size: u16,
    /// Associated Completion Queue ID (`CQID`).
    pub cqid: u16,
    /// Current head index.
    pub head: u16,
    /// Current tail index.
    pub tail: u16,
    /// Whether this queue has been created/enabled.
    pub valid: bool,
}

/// State of an NVMe Completion Queue (`CQ`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NvmeCq {
    /// Base guest physical address (`PRP1` / `ACQ`).
    pub base: u64,
    /// Number of entries (`1..=64`).
    pub size: u16,
    /// Current head index.
    pub head: u16,
    /// Current tail index.
    pub tail: u16,
    /// Current phase tag bit (`0` or `1`, starts at `1`).
    pub phase: u8,
    /// Interrupt enabled (`IEN`).
    pub irq_enabled: bool,
    /// Whether an unread completion is pending on this CQ.
    pub irq_pending: bool,
    /// Whether this queue has been created/enabled.
    pub valid: bool,
}

/// Serializable snapshot of an [`NvmeController`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NvmeSnapshot {
    /// PCI Command register.
    pub command: u16,
    /// PCI BAR0 register.
    pub bar0: u32,
    /// PCI Interrupt Line register.
    pub irq_line: u8,
    /// NVMe `INTMS` mask.
    pub intms: u32,
    /// NVMe `CC` register.
    pub cc: u32,
    /// NVMe `CSTS` register.
    pub csts: u32,
    /// NVMe `AQA` register.
    pub aqa: u32,
    /// NVMe `ASQ` register.
    pub asq: u64,
    /// NVMe `ACQ` register.
    pub acq: u64,
    /// Submission queues (`0..MAX_QUEUES`).
    pub sqs: [NvmeSq; MAX_QUEUES],
    /// Completion queues (`0..MAX_QUEUES`).
    pub cqs: [NvmeCq; MAX_QUEUES],
}

/// PCIe NVMe 1.4 Controller function.
pub struct NvmeController {
    pub(crate) slot: u8,
    backend: Arc<dyn BlockBackend>,
    serial: [u8; 20],
    irq: Arc<dyn IrqLine>,
    pub(crate) command: u16,
    pub(crate) bar0: u32,
    pub(crate) irq_line: u8,
    intms: u32,
    cc: u32,
    csts: u32,
    aqa: u32,
    asq: u64,
    acq: u64,
    sqs: [NvmeSq; MAX_QUEUES],
    cqs: [NvmeCq; MAX_QUEUES],
}

impl fmt::Debug for NvmeController {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NvmeController")
            .field("slot", &self.slot)
            .field("bar0", &format_args!("{:#x}", self.bar0))
            .field("cc", &format_args!("{:#x}", self.cc))
            .field("csts", &format_args!("{:#x}", self.csts))
            .finish_non_exhaustive()
    }
}

impl NvmeController {
    /// Creates a new PCIe NVMe 1.4 controller at `slot` backed by `backend`.
    pub fn new(
        slot: u8,
        backend: Arc<dyn BlockBackend>,
        serial_str: &str,
        irq: Arc<dyn IrqLine>,
    ) -> Self {
        let mut serial = [b' '; 20];
        let bytes = serial_str.as_bytes();
        let n = bytes.len().min(serial.len());
        serial[..n].copy_from_slice(&bytes[..n]);
        Self {
            slot,
            backend,
            serial,
            irq,
            command: 0,
            bar0: (PCIE_MMIO_BASE as u32) + u32::from(slot) * 0x1_0000,
            irq_line: 32 + (slot % 4),
            intms: 0,
            cc: 0,
            csts: 0,
            aqa: 0,
            asq: 0,
            acq: 0,
            sqs: [NvmeSq::default(); MAX_QUEUES],
            cqs: [NvmeCq::default(); MAX_QUEUES],
        }
    }

    /// Captures a snapshot of the NVMe controller state.
    pub fn snapshot(&self) -> NvmeSnapshot {
        NvmeSnapshot {
            command: self.command,
            bar0: self.bar0,
            irq_line: self.irq_line,
            intms: self.intms,
            cc: self.cc,
            csts: self.csts,
            aqa: self.aqa,
            asq: self.asq,
            acq: self.acq,
            sqs: self.sqs,
            cqs: self.cqs,
        }
    }

    /// Restores the NVMe controller state from `snap`.
    pub fn restore(&mut self, snap: NvmeSnapshot) {
        self.command = snap.command;
        self.bar0 = snap.bar0;
        self.irq_line = snap.irq_line;
        self.intms = snap.intms;
        self.cc = snap.cc;
        self.csts = snap.csts;
        self.aqa = snap.aqa;
        self.asq = snap.asq;
        self.acq = snap.acq;
        self.sqs = snap.sqs;
        self.cqs = snap.cqs;
        self.update_irq();
    }

    fn reset_controller(&mut self) {
        self.csts = 0;
        self.sqs = [NvmeSq::default(); MAX_QUEUES];
        self.cqs = [NvmeCq::default(); MAX_QUEUES];
        self.update_irq();
    }

    fn enable_controller(&mut self) {
        let asqs = ((self.aqa & 0x0fff) as u16).saturating_add(1).max(2);
        let acqs = (((self.aqa >> 16) & 0x0fff) as u16)
            .saturating_add(1)
            .max(2);
        self.sqs[0] = NvmeSq {
            base: self.asq & !0xfff,
            size: asqs,
            cqid: 0,
            head: 0,
            tail: 0,
            valid: true,
        };
        self.cqs[0] = NvmeCq {
            base: self.acq & !0xfff,
            size: acqs,
            head: 0,
            tail: 0,
            phase: 1,
            irq_enabled: true,
            irq_pending: false,
            valid: true,
        };
        self.csts = 1; // CSTS.RDY = 1
    }

    fn update_irq(&self) {
        let any_pending = (self.intms & 1 == 0)
            && self
                .cqs
                .iter()
                .any(|cq| cq.valid && cq.irq_enabled && cq.irq_pending);
        self.irq.set_level(any_pending);
    }

    fn config_byte(&self, reg: u16) -> u8 {
        match reg {
            // Vendor ID: 0x1b36 (Red Hat / QEMU)
            0x00 => 0x36,
            0x01 => 0x1b,
            // Device ID: 0x0010 (QEMU NVM Express Controller)
            0x02 => 0x10,
            0x03 => 0x00,
            // Command
            0x04 => (self.command & 0xff) as u8,
            0x05 => (self.command >> 8) as u8,
            // Status
            0x06 => 0x00,
            0x07 => 0x00,
            // Revision ID = 2, Programming Interface = 2 (NVMe), Subclass = 8, Class = 1
            0x08 => 0x02,
            0x09 => 0x02,
            0x0a => 0x08,
            0x0b => 0x01,
            // BAR0 (32-bit Memory BAR, 16 KiB)
            0x10..=0x13 => ((self.bar0 >> (8 * (reg - 0x10))) & 0xff) as u8,
            // Subsystem Vendor ID & Subsystem ID
            0x2c => 0xf4,
            0x2d => 0x1a,
            0x2e => 0x01,
            0x2f => 0x11,
            // Interrupt Line & Pin (INTA# = 1)
            0x3c => self.irq_line,
            0x3d => 0x01,
            _ => 0,
        }
    }

    pub(crate) fn read_config_space(&self, reg: u16, size: AccessSize) -> u64 {
        let n = size.bytes() as u16;
        let mut v = 0u64;
        for i in 0..n {
            v |= u64::from(self.config_byte(reg + i)) << (8 * i);
        }
        v
    }

    pub(crate) fn write_config_space(&mut self, reg: u16, size: AccessSize, value: u64) {
        if reg == 0x04 {
            self.command = value as u16;
        } else if reg == 0x10 && size == AccessSize::Word {
            let v = value as u32;
            if v == 0xffff_ffff {
                self.bar0 = !(VIRTIO_PCI_BAR0_SIZE - 1) & 0xffff_fff0;
            } else {
                self.bar0 = v & !(VIRTIO_PCI_BAR0_SIZE - 1);
            }
        } else if reg == 0x3c {
            self.irq_line = (value & 0xff) as u8;
        }
    }

    pub(crate) fn read_bar0(&self, offset: u64, size: AccessSize) -> u64 {
        // CAP: MQES=63 (bits 15:0), CQR=1 (bit 16), TO=20 (bits 31:24), CSS=1 (bit 37)
        const CAP: u64 = 63 | (1 << 16) | (20 << 24) | (1u64 << 37);
        const VS: u64 = 0x0001_0400; // NVMe 1.4.0

        match (offset, size) {
            (0x0000, AccessSize::Double) => CAP,
            (0x0000, _) => CAP & 0xffff_ffff,
            (0x0004, _) => CAP >> 32,
            (0x0008, _) => VS,
            (0x000c, _) | (0x0010, _) => u64::from(self.intms),
            (0x0014, _) => u64::from(self.cc),
            (0x001c, _) => u64::from(self.csts),
            (0x0024, _) => u64::from(self.aqa),
            (0x0028, AccessSize::Double) => self.asq,
            (0x0028, _) => self.asq & 0xffff_ffff,
            (0x002c, _) => self.asq >> 32,
            (0x0030, AccessSize::Double) => self.acq,
            (0x0030, _) => self.acq & 0xffff_ffff,
            (0x0034, _) => self.acq >> 32,
            _ => 0,
        }
    }

    pub(crate) fn write_bar0(&mut self, offset: u64, size: AccessSize, value: u64, dma: &DmaRam) {
        match offset {
            0x000c => {
                self.intms |= value as u32;
                self.update_irq();
            }
            0x0010 => {
                self.intms &= !(value as u32);
                self.update_irq();
            }
            0x0014 => {
                let prev_en = self.cc & 1 != 0;
                self.cc = value as u32;
                let now_en = self.cc & 1 != 0;
                let shn = (self.cc >> 14) & 3;
                if !now_en && prev_en {
                    self.reset_controller();
                } else if now_en && !prev_en {
                    self.enable_controller();
                }
                if shn != 0 {
                    // CSTS.SHST = 0b10 (Shutdown processing complete)
                    self.csts = (self.csts & !(3 << 2)) | (2 << 2);
                }
            }
            0x0024 => self.aqa = value as u32,
            0x0028 => {
                if size == AccessSize::Double {
                    self.asq = value;
                } else {
                    self.asq = (self.asq & !0xffff_ffff) | (value & 0xffff_ffff);
                }
            }
            0x002c => {
                self.asq = (self.asq & 0xffff_ffff) | ((value & 0xffff_ffff) << 32);
            }
            0x0030 => {
                if size == AccessSize::Double {
                    self.acq = value;
                } else {
                    self.acq = (self.acq & !0xffff_ffff) | (value & 0xffff_ffff);
                }
            }
            0x0034 => {
                self.acq = (self.acq & 0xffff_ffff) | ((value & 0xffff_ffff) << 32);
            }
            0x1000..=0x10ff => {
                let db_idx = ((offset - 0x1000) / 4) as usize;
                let qid = db_idx / 2;
                if qid >= MAX_QUEUES {
                    return;
                }
                if db_idx % 2 == 0 {
                    // Submission Queue `qid` Tail Doorbell (`SQyTDBL`)
                    if self.sqs[qid].valid && self.sqs[qid].size > 0 {
                        self.sqs[qid].tail = (value as u16) % self.sqs[qid].size;
                        self.process_sq(qid, dma);
                    }
                } else {
                    // Completion Queue `qid` Head Doorbell (`CQyHDBL`)
                    if self.cqs[qid].valid && self.cqs[qid].size > 0 {
                        self.cqs[qid].head = (value as u16) % self.cqs[qid].size;
                        if self.cqs[qid].head == self.cqs[qid].tail {
                            self.cqs[qid].irq_pending = false;
                        }
                        self.update_irq();
                    }
                }
            }
            _ => {}
        }
    }

    fn process_sq(&mut self, qid: usize, dma: &DmaRam) {
        while self.sqs[qid].valid && self.sqs[qid].head != self.sqs[qid].tail {
            let sq = self.sqs[qid];
            let sqe_addr = sq.base + u64::from(sq.head) * 64;
            self.sqs[qid].head = (sq.head + 1) % sq.size;

            let mut sqe = [0u8; 64];
            if dma.read_bytes(sqe_addr, &mut sqe).is_none() {
                continue;
            }
            let opcode = sqe[0];
            let cid = u16::from_le_bytes([sqe[2], sqe[3]]);
            let nsid = u32::from_le_bytes(sqe[4..8].try_into().unwrap());
            let prp1 = u64::from_le_bytes(sqe[24..32].try_into().unwrap());
            let prp2 = u64::from_le_bytes(sqe[32..40].try_into().unwrap());
            let cdw10 = u32::from_le_bytes(sqe[40..44].try_into().unwrap());
            let cdw11 = u32::from_le_bytes(sqe[44..48].try_into().unwrap());
            let cdw12 = u32::from_le_bytes(sqe[48..52].try_into().unwrap());

            let (dw0, status) = if qid == 0 {
                self.exec_admin_cmd(opcode, nsid, prp1, prp2, cdw10, cdw11, dma)
            } else {
                self.exec_io_cmd(opcode, nsid, prp1, prp2, cdw10, cdw11, cdw12, dma)
            };
            self.post_cqe(
                usize::from(sq.cqid),
                qid as u16,
                self.sqs[qid].head,
                cid,
                dw0,
                status,
                dma,
            );
        }
        self.update_irq();
    }

    #[allow(clippy::too_many_arguments)]
    fn post_cqe(
        &mut self,
        cqid: usize,
        sqid: u16,
        sq_head: u16,
        cid: u16,
        dw0: u32,
        status: u16,
        dma: &DmaRam,
    ) {
        if cqid >= MAX_QUEUES || !self.cqs[cqid].valid || self.cqs[cqid].size == 0 {
            return;
        }
        let cq = &mut self.cqs[cqid];
        let cqe_addr = cq.base + u64::from(cq.tail) * 16;
        let status_and_phase = (status << 1) | u16::from(cq.phase & 1);
        let mut cqe = [0u8; 16];
        cqe[0..4].copy_from_slice(&dw0.to_le_bytes());
        cqe[8..10].copy_from_slice(&sq_head.to_le_bytes());
        cqe[10..12].copy_from_slice(&sqid.to_le_bytes());
        cqe[12..14].copy_from_slice(&cid.to_le_bytes());
        cqe[14..16].copy_from_slice(&status_and_phase.to_le_bytes());
        let _ = dma.write_bytes(cqe_addr, &cqe);

        cq.tail += 1;
        if cq.tail >= cq.size {
            cq.tail = 0;
            cq.phase ^= 1;
        }
        cq.irq_pending = true;
    }

    #[allow(clippy::too_many_arguments)]
    fn exec_admin_cmd(
        &mut self,
        opcode: u8,
        nsid: u32,
        prp1: u64,
        prp2: u64,
        cdw10: u32,
        cdw11: u32,
        dma: &DmaRam,
    ) -> (u32, u16) {
        match opcode {
            // Delete I/O Submission Queue
            0x00 => {
                let qid = (cdw10 & 0xffff) as usize;
                if qid > 0 && qid < MAX_QUEUES {
                    self.sqs[qid] = NvmeSq::default();
                    (0, 0)
                } else {
                    (0, 0x0101) // Invalid Queue Identifier
                }
            }
            // Create I/O Submission Queue
            0x01 => {
                let qid = (cdw10 & 0xffff) as usize;
                let qsize = ((cdw10 >> 16) as u16).saturating_add(1);
                let cqid = (cdw11 >> 16) as u16;
                if qid > 0 && qid < MAX_QUEUES && usize::from(cqid) < MAX_QUEUES && qsize >= 2 {
                    self.sqs[qid] = NvmeSq {
                        base: prp1 & !0xfff,
                        size: qsize,
                        cqid,
                        head: 0,
                        tail: 0,
                        valid: true,
                    };
                    (0, 0)
                } else {
                    (0, 0x0101)
                }
            }
            // Get Log Page
            0x02 => {
                let numdl = (cdw10 >> 16) + 1;
                let bytes = (u64::from(numdl) * 4).min(4096) as usize;
                let zeroes = vec![0u8; bytes];
                let _ = Self::write_prp(dma, prp1, prp2, &zeroes);
                (0, 0)
            }
            // Delete I/O Completion Queue
            0x04 => {
                let qid = (cdw10 & 0xffff) as usize;
                if qid > 0 && qid < MAX_QUEUES {
                    self.cqs[qid] = NvmeCq::default();
                    (0, 0)
                } else {
                    (0, 0x0101)
                }
            }
            // Create I/O Completion Queue
            0x05 => {
                let qid = (cdw10 & 0xffff) as usize;
                let qsize = ((cdw10 >> 16) as u16).saturating_add(1);
                let ien = (cdw11 & 2) != 0;
                if qid > 0 && qid < MAX_QUEUES && qsize >= 2 {
                    self.cqs[qid] = NvmeCq {
                        base: prp1 & !0xfff,
                        size: qsize,
                        head: 0,
                        tail: 0,
                        phase: 1,
                        irq_enabled: ien,
                        irq_pending: false,
                        valid: true,
                    };
                    (0, 0)
                } else {
                    (0, 0x0101)
                }
            }
            // Identify
            0x06 => {
                let cns = cdw10 & 0xff;
                let mut buf = vec![0u8; 4096];
                match cns {
                    // Identify Namespace (NSID 1)
                    0x00 => {
                        if nsid != 1 {
                            return (0, 0x000b); // Invalid Namespace or Format
                        }
                        let sectors = self.backend.capacity_sectors();
                        buf[0..8].copy_from_slice(&sectors.to_le_bytes()); // NSZE
                        buf[8..16].copy_from_slice(&sectors.to_le_bytes()); // NCAP
                        buf[16..24].copy_from_slice(&sectors.to_le_bytes()); // NUSE
                        // LBAF0 @ offset 128: LBADS = 9 (2^9 = 512 bytes)
                        buf[130] = 9;
                    }
                    // Identify Controller
                    0x01 => {
                        buf[0..2].copy_from_slice(&0x1b36u16.to_le_bytes()); // VID
                        buf[2..4].copy_from_slice(&0x1af4u16.to_le_bytes()); // SSVID
                        buf[4..24].copy_from_slice(&self.serial); // SN
                        let model = b"rivemu NVMe 1.4 Controller              ";
                        buf[24..64].copy_from_slice(model); // MN
                        buf[64..72].copy_from_slice(b"1.0     "); // FR
                        buf[77] = 5; // MDTS = 5 (2^5 * 4 KiB = 128 KiB max transfer)
                        buf[80..84].copy_from_slice(&0x0001_0400u32.to_le_bytes()); // VER = 1.4.0
                        buf[512] = 0x66; // SQES: min=6 (64B), max=6 (64B)
                        buf[513] = 0x44; // CQES: min=4 (16B), max=4 (16B)
                        buf[516..520].copy_from_slice(&1u32.to_le_bytes()); // NN = 1 namespace
                        buf[524] = 1; // VWC: Volatile Write Cache present
                    }
                    // Active Namespace ID list
                    0x02 => {
                        buf[0..4].copy_from_slice(&1u32.to_le_bytes());
                    }
                    // Namespace Identification Descriptor list (CNS = 0x03)
                    0x03 => {
                        // Type 3 (UUID), Length 16
                        buf[0] = 3;
                        buf[1] = 16;
                        buf[4..20].copy_from_slice(&self.serial[..16]);
                    }
                    _ => {}
                }
                if Self::write_prp(dma, prp1, prp2, &buf) {
                    (0, 0)
                } else {
                    (0, 0x0004) // Data Transfer Error
                }
            }
            // Abort
            0x08 => (0, 0),
            // Set Features / Get Features
            0x09 | 0x0a => {
                let fid = cdw10 & 0xff;
                match fid {
                    // Number of Queues (`0x07`): advertise 15 I/O SQs and 15 I/O CQs (0-based = 14)
                    0x07 => (((14u32) << 16) | 14u32, 0),
                    // Volatile Write Cache (`0x06`)
                    0x06 => (1, 0),
                    _ => (0, 0),
                }
            }
            _ => (0, 0x0001), // Invalid Command Opcode
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn exec_io_cmd(
        &mut self,
        opcode: u8,
        nsid: u32,
        prp1: u64,
        prp2: u64,
        cdw10: u32,
        cdw11: u32,
        cdw12: u32,
        dma: &DmaRam,
    ) -> (u32, u16) {
        if nsid != 1 {
            return (0, 0x000b);
        }
        match opcode {
            // Flush
            0x00 => match self.backend.flush() {
                Ok(()) => (0, 0),
                Err(_) => (0, 0x0006),
            },
            // Write
            0x01 => {
                if self.backend.is_read_only() {
                    return (0, 0x0182); // Attempted Write to Read Only Range
                }
                let slba = u64::from(cdw10) | (u64::from(cdw11) << 32);
                let nlb = u64::from(cdw12 & 0xffff) + 1;
                let byte_len = (nlb * SECTOR_SIZE) as usize;
                let mut data = vec![0u8; byte_len];
                if !Self::read_prp(dma, prp1, prp2, &mut data) {
                    return (0, 0x0004);
                }
                match self.backend.write_at(slba * SECTOR_SIZE, &data) {
                    Ok(()) => (0, 0),
                    Err(_) => (0, 0x0004),
                }
            }
            // Read
            0x02 => {
                let slba = u64::from(cdw10) | (u64::from(cdw11) << 32);
                let nlb = u64::from(cdw12 & 0xffff) + 1;
                let byte_len = (nlb * SECTOR_SIZE) as usize;
                let mut data = vec![0u8; byte_len];
                if self.backend.read_at(slba * SECTOR_SIZE, &mut data).is_err() {
                    return (0, 0x0004);
                }
                if Self::write_prp(dma, prp1, prp2, &data) {
                    (0, 0)
                } else {
                    (0, 0x0004)
                }
            }
            _ => (0, 0x0001),
        }
    }

    fn read_prp(dma: &DmaRam, prp1: u64, prp2: u64, buf: &mut [u8]) -> bool {
        let first_len = ((PAGE_SIZE - (prp1 & 0xfff)) as usize).min(buf.len());
        if dma.read_bytes(prp1, &mut buf[..first_len]).is_none() {
            return false;
        }
        let rem = &mut buf[first_len..];
        if rem.is_empty() {
            return true;
        }
        if rem.len() <= PAGE_SIZE as usize {
            return dma.read_bytes(prp2 & !0xfff, rem).is_some();
        }
        let mut off = 0usize;
        let mut idx = 0u64;
        while off < rem.len() {
            let mut ptr_bytes = [0u8; 8];
            if dma.read_bytes(prp2 + idx * 8, &mut ptr_bytes).is_none() {
                return false;
            }
            let page_gpa = u64::from_le_bytes(ptr_bytes) & !0xfff;
            let chunk = (rem.len() - off).min(PAGE_SIZE as usize);
            if dma
                .read_bytes(page_gpa, &mut rem[off..off + chunk])
                .is_none()
            {
                return false;
            }
            off += chunk;
            idx += 1;
        }
        true
    }

    fn write_prp(dma: &DmaRam, prp1: u64, prp2: u64, buf: &[u8]) -> bool {
        let first_len = ((PAGE_SIZE - (prp1 & 0xfff)) as usize).min(buf.len());
        if dma.write_bytes(prp1, &buf[..first_len]).is_none() {
            return false;
        }
        let rem = &buf[first_len..];
        if rem.is_empty() {
            return true;
        }
        if rem.len() <= PAGE_SIZE as usize {
            return dma.write_bytes(prp2 & !0xfff, rem).is_some();
        }
        let mut off = 0usize;
        let mut idx = 0u64;
        while off < rem.len() {
            let mut ptr_bytes = [0u8; 8];
            if dma.read_bytes(prp2 + idx * 8, &mut ptr_bytes).is_none() {
                return false;
            }
            let page_gpa = u64::from_le_bytes(ptr_bytes) & !0xfff;
            let chunk = (rem.len() - off).min(PAGE_SIZE as usize);
            if dma.write_bytes(page_gpa, &rem[off..off + chunk]).is_none() {
                return false;
            }
            off += chunk;
            idx += 1;
        }
        true
    }
}
