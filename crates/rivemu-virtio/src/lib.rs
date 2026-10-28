//! VirtIO MMIO v2 transport, split virtqueues and `blk`/`net`/`console`/`rng` devices.

#![forbid(unsafe_code)]

pub mod blk;
pub mod console;
pub mod net;
pub mod queue;
pub mod rng;

use std::fmt;
use std::sync::{Arc, Mutex};

use rivemu_core::{AccessSize, IrqLine};
use rivemu_mem::MmioDevice;

pub use blk::VirtioBlk;
pub use console::VirtioConsole;
pub use net::VirtioNet;
pub use queue::{DmaRam, QUEUE_MAX_SIZE, VirtQueue};
pub use rng::VirtioRng;

/// VirtIO MMIO magic value (`"virt"`).
pub const VIRTIO_MMIO_MAGIC: u32 = 0x7472_6976;
/// VirtIO MMIO v2 version number.
pub const VIRTIO_MMIO_VERSION: u32 = 2;
/// Vendor ID (`"QEMU"`).
pub const VIRTIO_MMIO_VENDOR: u32 = 0x554d_4551;

/// Common VirtIO feature bits offered by the transport.
pub const VIRTIO_RING_F_INDIRECT_DESC: u64 = 1 << 28;
/// `avail_event` / `used_event` notification suppression.
pub const VIRTIO_RING_F_EVENT_IDX: u64 = 1 << 29;
/// VirtIO v1.0 compliant device.
pub const VIRTIO_F_VERSION_1: u64 = 1 << 32;

const INT_VRING: u32 = 1 << 0;

/// Trait implemented by specific VirtIO devices (`blk`, `net`, `console`, `rng`).
pub trait VirtioDevice: Send + Sync + fmt::Debug {
    /// VirtIO device ID (`1 = net`, `2 = blk`, `3 = console`, `4 = rng`).
    fn device_id(&self) -> u32;

    /// Number of virtqueues exposed by the device.
    fn num_queues(&self) -> usize;

    /// Device-specific feature bits (combined with transport feature bits).
    fn device_features(&self) -> u64;

    /// Reads from the device-specific configuration space (`0x100..`).
    fn read_config(&self, offset: u64, size: AccessSize) -> u64;

    /// Writes to the device-specific configuration space (`0x100..`).
    fn write_config(&mut self, offset: u64, size: AccessSize, value: u64);

    /// Processes available descriptors on `queue_idx`, returning the number of
    /// used descriptors produced.
    fn process_queue(
        &mut self,
        queue_idx: usize,
        queue: &mut VirtQueue,
        dma: &DmaRam,
        event_idx: bool,
    ) -> u32;

    /// Polls asynchronous host backends (e.g. network or console RX).
    fn poll(&mut self, _queues: &mut [VirtQueue], _dma: &DmaRam, _event_idx: bool) -> u32 {
        0
    }
}

/// VirtIO MMIO v2 transport wrapping a [`VirtioDevice`].
pub struct VirtioMmio {
    dma: DmaRam,
    irq: Arc<dyn IrqLine>,
    state: Mutex<MmioState>,
}

impl fmt::Debug for VirtioMmio {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VirtioMmio")
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
struct MmioState {
    dev: Box<dyn VirtioDevice>,
    queues: Vec<VirtQueue>,
    status: u32,
    device_features_sel: u32,
    driver_features_sel: u32,
    driver_features: u64,
    queue_sel: u32,
    interrupt_status: u32,
    config_generation: u32,
}

impl VirtioMmio {
    /// Creates a new `virtio-mmio` v2 transport connected to `irq` and `dma`.
    pub fn new(dev: Box<dyn VirtioDevice>, dma: DmaRam, irq: Arc<dyn IrqLine>) -> Arc<Self> {
        let num_q = dev.num_queues();
        Arc::new(Self {
            dma,
            irq,
            state: Mutex::new(MmioState {
                dev,
                queues: vec![VirtQueue::default(); num_q],
                status: 0,
                device_features_sel: 0,
                driver_features_sel: 0,
                driver_features: 0,
                queue_sel: 0,
                interrupt_status: 0,
                config_generation: 0,
            }),
        })
    }

    /// Polls the underlying device for asynchronous host events (RX packets/bytes).
    pub fn poll(&self) {
        let mut st = self.state.lock().unwrap();
        let MmioState {
            dev,
            queues,
            driver_features,
            interrupt_status,
            ..
        } = &mut *st;
        let event_idx = *driver_features & VIRTIO_RING_F_EVENT_IDX != 0;
        let n = dev.poll(queues, &self.dma, event_idx);
        if n > 0 {
            *interrupt_status |= INT_VRING;
            let level = *interrupt_status != 0;
            drop(st);
            self.irq.set_level(level);
        }
    }
}

impl MmioDevice for VirtioMmio {
    fn read(&self, offset: u64, size: AccessSize) -> u64 {
        let st = self.state.lock().unwrap();
        if offset >= 0x100 {
            return st.dev.read_config(offset - 0x100, size);
        }
        if size != AccessSize::Word {
            return 0;
        }
        let q = st.queues.get(st.queue_sel as usize);
        let val: u32 = match offset {
            0x000 => VIRTIO_MMIO_MAGIC,
            0x004 => VIRTIO_MMIO_VERSION,
            0x008 => st.dev.device_id(),
            0x00c => VIRTIO_MMIO_VENDOR,
            0x010 => {
                let feats = st.dev.device_features()
                    | VIRTIO_F_VERSION_1
                    | VIRTIO_RING_F_INDIRECT_DESC
                    | VIRTIO_RING_F_EVENT_IDX;
                match st.device_features_sel {
                    0 => feats as u32,
                    1 => (feats >> 32) as u32,
                    _ => 0,
                }
            }
            0x034 => q.map_or(0, |_| QUEUE_MAX_SIZE),
            0x044 => q.map_or(0, |q| u32::from(q.ready)),
            0x060 => st.interrupt_status,
            0x070 => st.status,
            0x0fc => st.config_generation,
            _ => 0,
        };
        u64::from(val)
    }

    fn write(&self, offset: u64, size: AccessSize, value: u64) {
        let mut st = self.state.lock().unwrap();
        if offset >= 0x100 {
            st.dev.write_config(offset - 0x100, size, value);
            return;
        }
        if size != AccessSize::Word {
            return;
        }
        let v = value as u32;
        let q_idx = st.queue_sel as usize;
        match offset {
            0x014 => st.device_features_sel = v,
            0x020 => {
                if st.driver_features_sel == 0 {
                    st.driver_features = (st.driver_features & !0xffff_ffff) | u64::from(v);
                } else if st.driver_features_sel == 1 {
                    st.driver_features = (st.driver_features & 0xffff_ffff) | (u64::from(v) << 32);
                }
            }
            0x024 => st.driver_features_sel = v,
            0x030 => st.queue_sel = v,
            0x038 => {
                if let Some(q) = st.queues.get_mut(q_idx) {
                    q.num = v.min(QUEUE_MAX_SIZE);
                }
            }
            0x044 => {
                if let Some(q) = st.queues.get_mut(q_idx) {
                    q.ready = v != 0;
                    if !q.ready {
                        q.last_avail_idx = 0;
                    }
                }
            }
            0x050 => {
                let notify_q = v as usize;
                let MmioState {
                    dev,
                    queues,
                    driver_features,
                    interrupt_status,
                    ..
                } = &mut *st;
                if let Some(q) = queues.get_mut(notify_q) {
                    let event_idx = *driver_features & VIRTIO_RING_F_EVENT_IDX != 0;
                    if dev.process_queue(notify_q, q, &self.dma, event_idx) > 0 {
                        *interrupt_status |= INT_VRING;
                    }
                }
            }
            0x064 => {
                st.interrupt_status &= !v;
            }
            0x070 => {
                if v == 0 {
                    // Device reset.
                    st.status = 0;
                    st.driver_features = 0;
                    st.interrupt_status = 0;
                    for q in &mut st.queues {
                        q.reset();
                    }
                } else {
                    st.status = v;
                }
            }
            0x080 => {
                if let Some(q) = st.queues.get_mut(q_idx) {
                    q.desc_addr = (q.desc_addr & !0xffff_ffff) | u64::from(v);
                }
            }
            0x084 => {
                if let Some(q) = st.queues.get_mut(q_idx) {
                    q.desc_addr = (q.desc_addr & 0xffff_ffff) | (u64::from(v) << 32);
                }
            }
            0x090 => {
                if let Some(q) = st.queues.get_mut(q_idx) {
                    q.avail_addr = (q.avail_addr & !0xffff_ffff) | u64::from(v);
                }
            }
            0x094 => {
                if let Some(q) = st.queues.get_mut(q_idx) {
                    q.avail_addr = (q.avail_addr & 0xffff_ffff) | (u64::from(v) << 32);
                }
            }
            0x0a0 => {
                if let Some(q) = st.queues.get_mut(q_idx) {
                    q.used_addr = (q.used_addr & !0xffff_ffff) | u64::from(v);
                }
            }
            0x0a4 => {
                if let Some(q) = st.queues.get_mut(q_idx) {
                    q.used_addr = (q.used_addr & 0xffff_ffff) | (u64::from(v) << 32);
                }
            }
            _ => {}
        }
        let irq_level = st.interrupt_status != 0;
        drop(st);
        self.irq.set_level(irq_level);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queue::{VIRTQ_DESC_F_NEXT, VIRTQ_DESC_F_WRITE};
    use rivemu_backends::{BlockBackend, BufferChardev, MemoryBlock};
    use rivemu_mem::Ram;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[derive(Debug, Default)]
    struct TestIrq(AtomicBool);

    impl IrqLine for TestIrq {
        fn set_level(&self, high: bool) {
            self.0.store(high, Ordering::SeqCst);
        }
    }

    fn setup_queue(mmio: &VirtioMmio, desc: u64, avail: u64, used: u64) {
        mmio.write(0x030, AccessSize::Word, 0); // QueueSel = 0
        mmio.write(0x038, AccessSize::Word, 16); // QueueNum = 16
        mmio.write(0x080, AccessSize::Word, desc);
        mmio.write(0x084, AccessSize::Word, desc >> 32);
        mmio.write(0x090, AccessSize::Word, avail);
        mmio.write(0x094, AccessSize::Word, avail >> 32);
        mmio.write(0x0a0, AccessSize::Word, used);
        mmio.write(0x0a4, AccessSize::Word, used >> 32);
        mmio.write(0x044, AccessSize::Word, 1); // QueueReady = 1
    }

    fn write_desc(dma: &DmaRam, table: u64, idx: u16, addr: u64, len: u32, flags: u16, next: u16) {
        let base = table + u64::from(idx) * 16;
        let _ = dma.write_u64(base, addr);
        let _ = dma.write_u32(base + 8, len);
        let _ = dma.write_u16(base + 12, flags);
        let _ = dma.write_u16(base + 14, next);
    }

    #[test]
    fn virtio_rng_fills_buffer_and_raises_irq() {
        let ram = Arc::new(Ram::new(0x10000).unwrap());
        let dma = DmaRam::new(0x8000_0000, ram);
        let irq = Arc::new(TestIrq::default());
        let mmio = VirtioMmio::new(Box::new(VirtioRng::new(12345)), dma.clone(), irq.clone());

        assert_eq!(
            mmio.read(0x000, AccessSize::Word),
            u64::from(VIRTIO_MMIO_MAGIC)
        );
        assert_eq!(mmio.read(0x004, AccessSize::Word), 2);
        assert_eq!(mmio.read(0x008, AccessSize::Word), 4);

        let (desc, avail, used, buf) = (0x8000_1000, 0x8000_2000, 0x8000_3000, 0x8000_4000);
        setup_queue(&mmio, desc, avail, used);
        write_desc(&dma, desc, 0, buf, 32, VIRTQ_DESC_F_WRITE, 0);
        dma.write_u16(avail + 2, 1).unwrap(); // avail.idx = 1
        dma.write_u16(avail + 4, 0).unwrap(); // avail.ring[0] = 0

        mmio.write(0x050, AccessSize::Word, 0); // QueueNotify(0)
        assert!(irq.0.load(Ordering::SeqCst));
        assert_eq!(dma.read_u16(used + 2), Some(1));
        assert_eq!(dma.read_u32(used + 8), Some(32));
        let mut out = [0u8; 32];
        dma.read_bytes(buf, &mut out).unwrap();
        assert_ne!(out, [0u8; 32]);

        // Acknowledge interrupt.
        mmio.write(0x064, AccessSize::Word, 1);
        assert!(!irq.0.load(Ordering::SeqCst));
    }

    #[test]
    fn virtio_blk_write_and_read_sectors() {
        let ram = Arc::new(Ram::new(0x10000).unwrap());
        let dma = DmaRam::new(0x8000_0000, ram);
        let irq = Arc::new(TestIrq::default());
        let disk = MemoryBlock::new(4096, false);
        let mmio = VirtioMmio::new(
            Box::new(VirtioBlk::new(disk.clone(), "hd0")),
            dma.clone(),
            irq,
        );
        assert_eq!(mmio.read(0x100, AccessSize::Double), 8); // 8 sectors

        let (desc, avail, used) = (0x8000_1000, 0x8000_2000, 0x8000_3000);
        setup_queue(&mmio, desc, avail, used);

        // Request 1: VIRTIO_BLK_T_OUT (type = 1, sector = 2).
        let (hdr, data, status) = (0x8000_4000, 0x8000_4100, 0x8000_4300);
        dma.write_u32(hdr, 1).unwrap(); // VIRTIO_BLK_T_OUT
        dma.write_u32(hdr + 4, 0).unwrap();
        dma.write_u64(hdr + 8, 2).unwrap();
        let payload = [0xabu8; 512];
        dma.write_bytes(data, &payload).unwrap();
        dma.write_bytes(status, &[0xff]).unwrap();

        write_desc(&dma, desc, 0, hdr, 16, VIRTQ_DESC_F_NEXT, 1);
        write_desc(&dma, desc, 1, data, 512, VIRTQ_DESC_F_NEXT, 2);
        write_desc(&dma, desc, 2, status, 1, VIRTQ_DESC_F_WRITE, 0);
        dma.write_u16(avail + 2, 1).unwrap();
        dma.write_u16(avail + 4, 0).unwrap();
        mmio.write(0x050, AccessSize::Word, 0);

        let mut st_byte = [0xffu8; 1];
        dma.read_bytes(status, &mut st_byte).unwrap();
        assert_eq!(st_byte[0], 0); // VIRTIO_BLK_S_OK

        let mut verify = [0u8; 512];
        disk.read_at(2 * 512, &mut verify).unwrap();
        assert_eq!(verify, payload);
    }

    #[test]
    fn virtio_console_tx_and_rx() {
        let ram = Arc::new(Ram::new(0x10000).unwrap());
        let dma = DmaRam::new(0x8000_0000, ram);
        let irq = Arc::new(TestIrq::default());
        let ch = BufferChardev::new();
        let mmio = VirtioMmio::new(Box::new(VirtioConsole::new(ch.clone())), dma.clone(), irq);

        // Configure RX queue 0.
        let (desc0, avail0, used0, rx_buf) = (0x8000_1000, 0x8000_2000, 0x8000_3000, 0x8000_4000);
        setup_queue(&mmio, desc0, avail0, used0);
        write_desc(&dma, desc0, 0, rx_buf, 64, VIRTQ_DESC_F_WRITE, 0);
        dma.write_u16(avail0 + 2, 1).unwrap();
        dma.write_u16(avail0 + 4, 0).unwrap();

        ch.push_input(b"hello guest");
        mmio.poll();
        let mut got = [0u8; 11];
        dma.read_bytes(rx_buf, &mut got).unwrap();
        assert_eq!(&got, b"hello guest");
    }
}
