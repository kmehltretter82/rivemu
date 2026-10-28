//! Generic PCIe Host Bridge (`pci-host-ecam-generic`) and Modern VirtIO 1.0 PCI
//! transport (`virtio-pci`).
//!
//! Exposes:
//! - Slot `00:00.0`: Red Hat QEMU PCIe Host Bridge (`1b36:0008`, class `0x060000`)
//! - Slots `00:01.0..`: Modern VirtIO 1.0 PCI devices (`1af4:1040 + id`) with
//!   a 16 KiB 32-bit Memory BAR0 containing Common (`0x0000`), ISR (`0x1000`),
//!   Notify (`0x2000`), and Device-specific (`0x3000`) configuration regions.

use std::fmt;
use std::sync::{Arc, Mutex};

use rivemu_core::{AccessSize, IrqLine};
use rivemu_mem::MmioDevice;

use crate::{
    DmaRam, QUEUE_MAX_SIZE, VIRTIO_F_VERSION_1, VIRTIO_RING_F_EVENT_IDX,
    VIRTIO_RING_F_INDIRECT_DESC, VirtQueue, VirtioDevice,
};

/// Base of the 32-bit PCI MMIO window on the RISC-V `virt` board.
pub const PCIE_MMIO_BASE: u64 = 0x4000_0000;
/// Size of a `virtio-pci` BAR0 region (16 KiB).
pub const VIRTIO_PCI_BAR0_SIZE: u32 = 0x4000;

const INT_VRING: u8 = 1 << 0;

struct VirtioPciFunction {
    slot: u8,
    dev: Box<dyn VirtioDevice>,
    queues: Vec<VirtQueue>,
    irq: Arc<dyn IrqLine>,
    command: u16,
    bar0: u32,
    irq_line: u8,
    status: u8,
    device_feature_sel: u32,
    driver_feature_sel: u32,
    driver_features: u64,
    queue_sel: u16,
    msix_config: u16,
    interrupt_status: u8,
    config_generation: u8,
}

impl fmt::Debug for VirtioPciFunction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VirtioPciFunction")
            .field("slot", &self.slot)
            .field("dev", &self.dev)
            .field("bar0", &format_args!("{:#x}", self.bar0))
            .finish_non_exhaustive()
    }
}

impl VirtioPciFunction {
    fn new(slot: u8, dev: Box<dyn VirtioDevice>, irq: Arc<dyn IrqLine>) -> Self {
        let num_q = dev.num_queues();
        Self {
            slot,
            dev,
            queues: vec![VirtQueue::default(); num_q],
            irq,
            command: 0,
            bar0: (PCIE_MMIO_BASE as u32) + u32::from(slot) * 0x1_0000,
            irq_line: 32 + (slot % 4),
            status: 0,
            device_feature_sel: 0,
            driver_feature_sel: 0,
            driver_features: 0,
            queue_sel: 0,
            msix_config: 0xffff,
            interrupt_status: 0,
            config_generation: 0,
        }
    }

    fn reset_virtio(&mut self) {
        self.status = 0;
        self.device_feature_sel = 0;
        self.driver_feature_sel = 0;
        self.driver_features = 0;
        self.queue_sel = 0;
        self.interrupt_status = 0;
        for q in &mut self.queues {
            q.reset();
        }
        self.irq.set_level(false);
    }

    fn class_code(&self) -> u32 {
        match self.dev.device_id() {
            1 => 0x02_0000,  // Network controller: Ethernet
            2 => 0x01_0000,  // Mass storage controller: SCSI
            3 => 0x07_8000,  // Communication controller
            4 => 0x00_ff00,  // Unclassified device (RNG)
            9 => 0x00_0200,  // Filesystem / 9P share
            16 => 0x03_8000, // Display controller: 2D GPU
            18 => 0x09_8000, // Input device controller: Keyboard/Tablet
            _ => 0x00_ff00,
        }
    }

    fn config_byte(&self, reg: u16) -> u8 {
        let dev_id = 0x1040u16 + self.dev.device_id() as u16;
        let subsys_id = 0x0040u16 + self.dev.device_id() as u16;
        let class = self.class_code();
        match reg {
            // Vendor ID: 0x1af4 (Red Hat / VirtIO)
            0x00 => 0xf4,
            0x01 => 0x1a,
            // Device ID: 0x1040 + VirtIO ID
            0x02 => (dev_id & 0xff) as u8,
            0x03 => (dev_id >> 8) as u8,
            // Command register
            0x04 => (self.command & 0xff) as u8,
            0x05 => (self.command >> 8) as u8,
            // Status register: bit 4 = Capabilities List (0x0010)
            0x06 => 0x10,
            0x07 => 0x00,
            // Revision ID (1 = Modern VirtIO 1.0) + 24-bit Class Code
            0x08 => 0x01,
            0x09 => (class & 0xff) as u8,
            0x0a => ((class >> 8) & 0xff) as u8,
            0x0b => ((class >> 16) & 0xff) as u8,
            // Header Type = 0x00
            0x0e => 0x00,
            // BAR0 (32-bit non-prefetchable Memory BAR, size 16 KiB)
            0x10..=0x13 => ((self.bar0 >> (8 * (reg - 0x10))) & 0xff) as u8,
            // Subsystem Vendor ID (0x1af4) & Subsystem ID
            0x2c => 0xf4,
            0x2d => 0x1a,
            0x2e => (subsys_id & 0xff) as u8,
            0x2f => (subsys_id >> 8) as u8,
            // Capabilities Pointer -> 0x40
            0x34 => 0x40,
            // Interrupt Line & Pin (INTA# = 1)
            0x3c => self.irq_line,
            0x3d => 0x01,

            // Cap 1 @ 0x40: VIRTIO_PCI_CAP_COMMON_CFG (type=1, bar=0, offset=0x0000, len=0x0038)
            0x40 => 0x09, // PCI_CAP_ID_VNDR
            0x41 => 0x50, // next
            0x42 => 16,   // cap_len
            0x43 => 1,    // VIRTIO_PCI_CAP_COMMON_CFG
            0x44 => 0,    // bar = 0
            0x4c => 0x38, // length = 0x38

            // Cap 2 @ 0x50: VIRTIO_PCI_CAP_ISR_CFG (type=3, bar=0, offset=0x1000, len=0x0004)
            0x50 => 0x09,
            0x51 => 0x60,
            0x52 => 16,
            0x53 => 3,    // VIRTIO_PCI_CAP_ISR_CFG
            0x54 => 0,    // bar = 0
            0x59 => 0x10, // offset = 0x1000
            0x5c => 0x04, // length = 0x04

            // Cap 3 @ 0x60: VIRTIO_PCI_CAP_NOTIFY_CFG (type=2, bar=0, offset=0x2000, len=0x1000, mult=4)
            0x60 => 0x09,
            0x61 => 0x74,
            0x62 => 20,
            0x63 => 2,    // VIRTIO_PCI_CAP_NOTIFY_CFG
            0x64 => 0,    // bar = 0
            0x69 => 0x20, // offset = 0x2000
            0x6d => 0x10, // length = 0x1000
            0x70 => 0x04, // notify_off_multiplier = 4

            // Cap 4 @ 0x74: VIRTIO_PCI_CAP_DEVICE_CFG (type=4, bar=0, offset=0x3000, len=0x1000)
            0x74 => 0x09,
            0x75 => 0x00,
            0x76 => 16,
            0x77 => 4,    // VIRTIO_PCI_CAP_DEVICE_CFG
            0x78 => 0,    // bar = 0
            0x7d => 0x30, // offset = 0x3000
            0x81 => 0x10, // length = 0x1000
            _ => 0,
        }
    }

    fn read_config_space(&self, reg: u16, size: AccessSize) -> u64 {
        let n = size.bytes() as u16;
        let mut v = 0u64;
        for i in 0..n {
            v |= u64::from(self.config_byte(reg + i)) << (8 * i);
        }
        v
    }

    fn write_config_space(&mut self, reg: u16, size: AccessSize, value: u64) {
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

    fn read_bar0(&mut self, offset: u64, size: AccessSize) -> u64 {
        match offset {
            // Common configuration (`0x0000..0x0038`)
            0x0000 => u64::from(self.device_feature_sel),
            0x0004 => {
                let feats = self.dev.device_features()
                    | VIRTIO_RING_F_INDIRECT_DESC
                    | VIRTIO_RING_F_EVENT_IDX
                    | VIRTIO_F_VERSION_1;
                match self.device_feature_sel {
                    0 => feats & 0xffff_ffff,
                    1 => (feats >> 32) & 0xffff_ffff,
                    _ => 0,
                }
            }
            0x0008 => u64::from(self.driver_feature_sel),
            0x000c => match self.driver_feature_sel {
                0 => self.driver_features & 0xffff_ffff,
                1 => (self.driver_features >> 32) & 0xffff_ffff,
                _ => 0,
            },
            0x0010 => u64::from(self.msix_config),
            0x0012 => self.queues.len() as u64,
            0x0014 => u64::from(self.status),
            0x0015 => u64::from(self.config_generation),
            0x0016 => u64::from(self.queue_sel),
            0x0018 => self
                .queues
                .get(self.queue_sel as usize)
                .map_or(0, |q| u64::from(q.num.max(QUEUE_MAX_SIZE))),
            0x001a => 0xffff,
            0x001c => self
                .queues
                .get(self.queue_sel as usize)
                .map_or(0, |q| u64::from(q.ready)),
            0x001e => u64::from(self.queue_sel),
            0x0020 => self
                .queues
                .get(self.queue_sel as usize)
                .map_or(0, |q| q.desc_addr & 0xffff_ffff),
            0x0024 => self
                .queues
                .get(self.queue_sel as usize)
                .map_or(0, |q| q.desc_addr >> 32),
            0x0028 => self
                .queues
                .get(self.queue_sel as usize)
                .map_or(0, |q| q.avail_addr & 0xffff_ffff),
            0x002c => self
                .queues
                .get(self.queue_sel as usize)
                .map_or(0, |q| q.avail_addr >> 32),
            0x0030 => self
                .queues
                .get(self.queue_sel as usize)
                .map_or(0, |q| q.used_addr & 0xffff_ffff),
            0x0034 => self
                .queues
                .get(self.queue_sel as usize)
                .map_or(0, |q| q.used_addr >> 32),

            // ISR status (`0x1000..0x1004`): reading clears interrupt status!
            0x1000..=0x1003 => {
                let isr = self.interrupt_status;
                self.interrupt_status = 0;
                self.irq.set_level(false);
                u64::from(isr)
            }

            // Device-specific config (`0x3000..0x4000`)
            0x3000..=0x3fff => self.dev.read_config(offset - 0x3000, size),
            _ => 0,
        }
    }

    fn write_bar0(&mut self, offset: u64, size: AccessSize, value: u64, dma: &DmaRam) {
        let q_idx = self.queue_sel as usize;
        match offset {
            0x0000 => self.device_feature_sel = value as u32,
            0x0008 => self.driver_feature_sel = value as u32,
            0x000c => {
                let v = value & 0xffff_ffff;
                match self.driver_feature_sel {
                    0 => self.driver_features = (self.driver_features & !0xffff_ffff) | v,
                    1 => self.driver_features = (self.driver_features & 0xffff_ffff) | (v << 32),
                    _ => {}
                }
            }
            0x0010 => self.msix_config = value as u16,
            0x0014 => {
                if (value & 0xff) == 0 {
                    self.reset_virtio();
                } else {
                    self.status = value as u8;
                }
            }
            0x0016 => self.queue_sel = value as u16,
            0x0018 => {
                if let Some(q) = self.queues.get_mut(q_idx) {
                    q.num = (value as u32).min(QUEUE_MAX_SIZE);
                }
            }
            0x001c => {
                if let Some(q) = self.queues.get_mut(q_idx) {
                    q.ready = (value & 1) != 0;
                }
            }
            0x0020 => {
                if let Some(q) = self.queues.get_mut(q_idx) {
                    if size == AccessSize::Double {
                        q.desc_addr = value;
                    } else {
                        q.desc_addr = (q.desc_addr & !0xffff_ffff) | (value & 0xffff_ffff);
                    }
                }
            }
            0x0024 => {
                if let Some(q) = self.queues.get_mut(q_idx) {
                    q.desc_addr = (q.desc_addr & 0xffff_ffff) | ((value & 0xffff_ffff) << 32);
                }
            }
            0x0028 => {
                if let Some(q) = self.queues.get_mut(q_idx) {
                    if size == AccessSize::Double {
                        q.avail_addr = value;
                    } else {
                        q.avail_addr = (q.avail_addr & !0xffff_ffff) | (value & 0xffff_ffff);
                    }
                }
            }
            0x002c => {
                if let Some(q) = self.queues.get_mut(q_idx) {
                    q.avail_addr = (q.avail_addr & 0xffff_ffff) | ((value & 0xffff_ffff) << 32);
                }
            }
            0x0030 => {
                if let Some(q) = self.queues.get_mut(q_idx) {
                    if size == AccessSize::Double {
                        q.used_addr = value;
                    } else {
                        q.used_addr = (q.used_addr & !0xffff_ffff) | (value & 0xffff_ffff);
                    }
                }
            }
            0x0034 => {
                if let Some(q) = self.queues.get_mut(q_idx) {
                    q.used_addr = (q.used_addr & 0xffff_ffff) | ((value & 0xffff_ffff) << 32);
                }
            }
            // Queue Notify (`0x2000..0x3000`, 4-byte stride per queue)
            0x2000..=0x2fff => {
                let notify_q = ((offset - 0x2000) / 4) as usize;
                let event_idx = self.driver_features & VIRTIO_RING_F_EVENT_IDX != 0;
                if let Some(q) = self
                    .queues
                    .get_mut(notify_q)
                    .filter(|q| q.ready && q.num > 0)
                {
                    let n = self.dev.process_queue(notify_q, q, dma, event_idx);
                    if n > 0 {
                        self.interrupt_status |= INT_VRING;
                        self.irq.set_level(true);
                    }
                }
            }
            // Device-specific config (`0x3000..0x4000`)
            0x3000..=0x3fff => self.dev.write_config(offset - 0x3000, size, value),
            _ => {}
        }
    }
}

/// Generic PCIe Host Bridge (`pci-host-ecam-generic`) managing Bus 0.
pub struct PciBus {
    dma: DmaRam,
    irqs: [Arc<dyn IrqLine>; 4],
    funcs: Mutex<Vec<VirtioPciFunction>>,
}

impl fmt::Debug for PciBus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PciBus")
            .field("funcs", &self.funcs)
            .finish_non_exhaustive()
    }
}

impl PciBus {
    /// Creates a new PCIe host bridge connected to `dma` and the 4 INTx PLIC lines (`IRQ 32..35`).
    pub fn new(dma: DmaRam, irqs: [Arc<dyn IrqLine>; 4]) -> Arc<Self> {
        Arc::new(Self {
            dma,
            irqs,
            funcs: Mutex::new(Vec::new()),
        })
    }

    /// Attaches a [`VirtioDevice`] as a Modern VirtIO 1.0 PCI function at the next slot (`00:01.0..`).
    pub fn add_virtio_device(&self, dev: Box<dyn VirtioDevice>) {
        let mut funcs = self.funcs.lock().unwrap();
        let slot = (funcs.len() as u8) + 1;
        if slot < 32 {
            // Standard PCI INTx swizzle for INTA# (pin 1): `(slot + 0) % 4`.
            let irq = self.irqs[(slot as usize) % 4].clone();
            funcs.push(VirtioPciFunction::new(slot, dev, irq));
        }
    }

    /// Returns the ECAM configuration space MMIO window (`0x3000_0000..0x4000_0000`).
    pub fn ecam_window(self: &Arc<Self>) -> Arc<PciEcamWindow> {
        Arc::new(PciEcamWindow(self.clone()))
    }

    /// Returns the 32-bit PCI MMIO BAR window (`0x4000_0000..0x8000_0000`).
    pub fn mmio_window(self: &Arc<Self>) -> Arc<PciMmioWindow> {
        Arc::new(PciMmioWindow(self.clone()))
    }

    /// Polls all attached `virtio-pci` devices for asynchronous host events.
    pub fn poll(&self) {
        let mut funcs = self.funcs.lock().unwrap();
        for f in funcs.iter_mut() {
            let event_idx = f.driver_features & VIRTIO_RING_F_EVENT_IDX != 0;
            let n = f.dev.poll(&mut f.queues, &self.dma, event_idx);
            if n > 0 {
                f.interrupt_status |= INT_VRING;
                f.irq.set_level(true);
            }
        }
    }

    fn host_bridge_config_byte(reg: u16) -> u8 {
        match reg {
            // Vendor ID: 0x1b36 (Red Hat, Inc.)
            0x00 => 0x36,
            0x01 => 0x1b,
            // Device ID: 0x0008 (QEMU PCIe Host bridge)
            0x02 => 0x08,
            0x03 => 0x00,
            // Status
            0x06 => 0x00,
            0x07 => 0x00,
            // Class Code: 0x060000 (Host bridge)
            0x0a => 0x00,
            0x0b => 0x06,
            _ => 0,
        }
    }
}

/// ECAM configuration space window (`0x3000_0000..0x4000_0000`).
#[derive(Debug)]
pub struct PciEcamWindow(Arc<PciBus>);

impl MmioDevice for PciEcamWindow {
    fn read(&self, offset: u64, size: AccessSize) -> u64 {
        let bus = (offset >> 20) & 0xff;
        let slot = ((offset >> 15) & 0x1f) as u8;
        let func = (offset >> 12) & 0x7;
        let reg = (offset & 0xfff) as u16;

        if bus != 0 || func != 0 {
            return size.mask();
        }
        if slot == 0 {
            let n = size.bytes() as u16;
            let mut v = 0u64;
            for i in 0..n {
                v |= u64::from(PciBus::host_bridge_config_byte(reg + i)) << (8 * i);
            }
            return v;
        }
        let funcs = self.0.funcs.lock().unwrap();
        let Some(f) = funcs.iter().find(|f| f.slot == slot) else {
            return size.mask();
        };
        f.read_config_space(reg, size)
    }

    fn write(&self, offset: u64, size: AccessSize, value: u64) {
        let bus = (offset >> 20) & 0xff;
        let slot = ((offset >> 15) & 0x1f) as u8;
        let func = (offset >> 12) & 0x7;
        let reg = (offset & 0xfff) as u16;
        if bus != 0 || func != 0 || slot == 0 {
            return;
        }
        let mut funcs = self.0.funcs.lock().unwrap();
        if let Some(f) = funcs.iter_mut().find(|f| f.slot == slot) {
            f.write_config_space(reg, size, value);
        }
    }
}

/// 32-bit PCI MMIO BAR window (`0x4000_0000..0x8000_0000`).
#[derive(Debug)]
pub struct PciMmioWindow(Arc<PciBus>);

impl MmioDevice for PciMmioWindow {
    fn read(&self, offset: u64, size: AccessSize) -> u64 {
        let gpa = PCIE_MMIO_BASE + offset;
        let mut funcs = self.0.funcs.lock().unwrap();
        for f in funcs.iter_mut() {
            let base = u64::from(f.bar0 & !0xf);
            if base >= PCIE_MMIO_BASE && gpa >= base && gpa < base + u64::from(VIRTIO_PCI_BAR0_SIZE)
            {
                return f.read_bar0(gpa - base, size);
            }
        }
        size.mask()
    }

    fn write(&self, offset: u64, size: AccessSize, value: u64) {
        let gpa = PCIE_MMIO_BASE + offset;
        let mut funcs = self.0.funcs.lock().unwrap();
        for f in funcs.iter_mut() {
            let base = u64::from(f.bar0 & !0xf);
            if base >= PCIE_MMIO_BASE && gpa >= base && gpa < base + u64::from(VIRTIO_PCI_BAR0_SIZE)
            {
                f.write_bar0(gpa - base, size, value, &self.0.dma);
                return;
            }
        }
    }
}
