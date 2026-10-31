//! VirtIO MMIO v2 transport, split virtqueues and `blk`/`net`/`console`/`rng` devices.

#![forbid(unsafe_code)]

pub mod blk;
pub mod console;
pub mod gpu;
pub mod input;
pub mod net;
pub mod nvme;
pub mod p9;
pub mod pci;
pub mod queue;
pub mod rng;
pub mod vnc;
pub mod vsock;

use std::fmt;
use std::sync::{Arc, Mutex};

use rivemu_core::{AccessSize, IrqLine};
use rivemu_mem::MmioDevice;

pub use blk::VirtioBlk;
pub use console::VirtioConsole;
pub use gpu::{DEFAULT_GPU_HEIGHT, DEFAULT_GPU_WIDTH, Framebuffer, VirtioGpu};
pub use input::{InputHub, VirtioInput, VirtioInputEvent};
pub use net::VirtioNet;
pub use nvme::{NvmeController, NvmeCq, NvmeSnapshot, NvmeSq};
pub use p9::Virtio9p;
pub use pci::{
    PCIE_MMIO_BASE, PciBus, PciEcamWindow, PciMmioWindow, VIRTIO_PCI_BAR0_SIZE, VirtioPciSnapshot,
};
pub use queue::{DmaRam, QUEUE_MAX_SIZE, VirtQueue};
pub use rng::VirtioRng;
pub use vnc::VncServer;
pub use vsock::{
    DEFAULT_GUEST_CID, VIRTIO_VSOCK_TYPE_STREAM, VMADDR_CID_HOST, VSOCK_HDR_SIZE, VirtioVsock,
    VsockHeader, VsockHostEndpoint, vsock_op,
};

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

    /// Exports transport and virtqueue state for VM snapshotting.
    #[allow(clippy::type_complexity)]
    pub fn snapshot(&self) -> ([u32; 6], u64, Vec<VirtQueue>) {
        let st = self.state.lock().unwrap();
        (
            [
                st.status,
                st.device_features_sel,
                st.driver_features_sel,
                st.queue_sel,
                st.interrupt_status,
                st.config_generation,
            ],
            st.driver_features,
            st.queues.clone(),
        )
    }

    /// Restores transport and virtqueue state from a VM snapshot.
    pub fn restore(&self, regs: [u32; 6], driver_features: u64, queues: Vec<VirtQueue>) {
        let mut st = self.state.lock().unwrap();
        st.status = regs[0];
        st.device_features_sel = regs[1];
        st.driver_features_sel = regs[2];
        st.queue_sel = regs[3];
        st.interrupt_status = regs[4];
        st.config_generation = regs[5];
        st.driver_features = driver_features;
        st.queues = queues;
        let level = st.interrupt_status != 0;
        drop(st);
        self.irq.set_level(level);
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
    use rivemu_mem::{AddressSpace, Ram};
    use std::sync::atomic::{AtomicBool, Ordering};

    #[derive(Debug, Default)]
    struct TestIrq(AtomicBool);

    impl IrqLine for TestIrq {
        fn set_level(&self, high: bool) {
            self.0.store(high, Ordering::SeqCst);
        }
    }

    fn test_dma(size: u64) -> DmaRam {
        let ram = Arc::new(Ram::new(size).unwrap());
        let mut bus = AddressSpace::new();
        bus.add_ram("ram", 0x8000_0000, ram.clone()).unwrap();
        DmaRam::new(0x8000_0000, ram, bus.reservation_invalidator())
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
        let dma = test_dma(0x10000);
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
        let dma = test_dma(0x10000);
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
        let dma = test_dma(0x10000);
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

    #[test]
    fn virtio_9p_version_attach_walk_and_read() {
        let dir = std::env::temp_dir().join(format!("rivemu-9p-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(dir.join("hello.txt"), b"9P_HOST_FILE_OK").unwrap();

        let dma = test_dma(0x10000);
        let irq = Arc::new(TestIrq::default());
        let mmio = VirtioMmio::new(
            Box::new(Virtio9p::new(&dir, "host0")),
            dma.clone(),
            irq.clone(),
        );

        assert_eq!(mmio.read(0x008, AccessSize::Word), 9); // VIRTIO_ID_9P
        assert_eq!(mmio.read(0x100, AccessSize::Half), 5); // tag_len = 5 ("host0")

        let (desc, avail, used, req_buf, resp_buf) = (
            0x8000_1000,
            0x8000_2000,
            0x8000_3000,
            0x8000_4000,
            0x8000_5000,
        );
        setup_queue(&mmio, desc, avail, used);

        let mut avail_idx = 0u16;
        let mut rpc = |req: &[u8]| -> Vec<u8> {
            dma.write_bytes(req_buf, req).unwrap();
            write_desc(
                &dma,
                desc,
                0,
                req_buf,
                req.len() as u32,
                VIRTQ_DESC_F_NEXT,
                1,
            );
            write_desc(&dma, desc, 1, resp_buf, 4096, VIRTQ_DESC_F_WRITE, 0);
            avail_idx += 1;
            dma.write_u16(avail + 2, avail_idx).unwrap();
            dma.write_u16(avail + 4 + u64::from(avail_idx - 1) * 2, 0)
                .unwrap();
            mmio.write(0x050, AccessSize::Word, 0);
            let len = dma.read_u32(resp_buf).unwrap() as usize;
            let mut out = vec![0u8; len];
            dma.read_bytes(resp_buf, &mut out).unwrap();
            out
        };

        // 1. Tversion (type = 100)
        let mut tver = vec![0, 0, 0, 0, 100, 0xff, 0xff];
        tver.extend_from_slice(&8192u32.to_le_bytes());
        tver.extend_from_slice(&8u16.to_le_bytes());
        tver.extend_from_slice(b"9P2000.L");
        let l = tver.len() as u32;
        tver[0..4].copy_from_slice(&l.to_le_bytes());
        let rver = rpc(&tver);
        assert_eq!(rver[4], 101); // Rversion

        // 2. Tattach (type = 104, fid = 1)
        let mut tatt = vec![0, 0, 0, 0, 104, 1, 0];
        tatt.extend_from_slice(&1u32.to_le_bytes()); // fid = 1
        tatt.extend_from_slice(&u32::MAX.to_le_bytes()); // afid = NOFID
        tatt.extend_from_slice(&0u16.to_le_bytes()); // uname = ""
        tatt.extend_from_slice(&0u16.to_le_bytes()); // aname = ""
        tatt.extend_from_slice(&0u32.to_le_bytes()); // n_uname = 0
        let l = tatt.len() as u32;
        tatt[0..4].copy_from_slice(&l.to_le_bytes());
        let ratt = rpc(&tatt);
        assert_eq!(ratt[4], 105); // Rattach

        // 3. Twalk (type = 110, fid = 1 -> newfid = 2, ["hello.txt"])
        let mut twalk = vec![0, 0, 0, 0, 110, 2, 0];
        twalk.extend_from_slice(&1u32.to_le_bytes());
        twalk.extend_from_slice(&2u32.to_le_bytes());
        twalk.extend_from_slice(&1u16.to_le_bytes());
        twalk.extend_from_slice(&9u16.to_le_bytes());
        twalk.extend_from_slice(b"hello.txt");
        let l = twalk.len() as u32;
        twalk[0..4].copy_from_slice(&l.to_le_bytes());
        let rwalk = rpc(&twalk);
        assert_eq!(rwalk[4], 111); // Rwalk

        // 4. Tread (type = 116, fid = 2, offset = 0, count = 64)
        let mut tread = vec![0, 0, 0, 0, 116, 3, 0];
        tread.extend_from_slice(&2u32.to_le_bytes());
        tread.extend_from_slice(&0u64.to_le_bytes());
        tread.extend_from_slice(&64u32.to_le_bytes());
        let l = tread.len() as u32;
        tread[0..4].copy_from_slice(&l.to_le_bytes());
        let rread = rpc(&tread);
        assert_eq!(rread[4], 117); // Rread
        let data_len = u32::from_le_bytes(rread[7..11].try_into().unwrap()) as usize;
        assert_eq!(&rread[11..11 + data_len], b"9P_HOST_FILE_OK");

        // 5. Tlcreate (type = 14, fid = 1, name = "guest.txt") + Twrite (type = 118)
        let mut tcreate = vec![0, 0, 0, 0, 14, 4, 0];
        tcreate.extend_from_slice(&1u32.to_le_bytes());
        tcreate.extend_from_slice(&9u16.to_le_bytes());
        tcreate.extend_from_slice(b"guest.txt");
        tcreate.extend_from_slice(&0u32.to_le_bytes()); // flags
        tcreate.extend_from_slice(&0o644u32.to_le_bytes()); // mode
        tcreate.extend_from_slice(&0u32.to_le_bytes()); // gid
        let l = tcreate.len() as u32;
        tcreate[0..4].copy_from_slice(&l.to_le_bytes());
        let rcreate = rpc(&tcreate);
        assert_eq!(rcreate[4], 15); // Rlcreate

        let msg = b"WRITTEN_OVER_VIRTIO_9P";
        let mut twrite = vec![0, 0, 0, 0, 118, 5, 0];
        twrite.extend_from_slice(&1u32.to_le_bytes()); // fid = 1 (now points to guest.txt)
        twrite.extend_from_slice(&0u64.to_le_bytes()); // offset = 0
        twrite.extend_from_slice(&(msg.len() as u32).to_le_bytes());
        twrite.extend_from_slice(msg);
        let l = twrite.len() as u32;
        twrite[0..4].copy_from_slice(&l.to_le_bytes());
        let rwrite = rpc(&twrite);
        assert_eq!(rwrite[4], 119); // Rwrite
        assert_eq!(std::fs::read(dir.join("guest.txt")).unwrap(), msg);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn virtio_gpu_2d_scanout_and_ppm_png_export() {
        let dma = test_dma(0x2_0000);
        let irq = Arc::new(TestIrq::default());
        let fb = Arc::new(Framebuffer::new(32, 16));
        let mmio = VirtioMmio::new(
            Box::new(VirtioGpu::new(32, 16, fb.clone())),
            dma.clone(),
            irq,
        );
        assert_eq!(mmio.read(0x008, AccessSize::Word), 16);
        assert_eq!(mmio.read(0x108, AccessSize::Word), 1); // num_scanouts = 1

        let (desc, avail, used, req_buf, resp_buf, backing_buf) = (
            0x8000_1000,
            0x8000_2000,
            0x8000_3000,
            0x8000_4000,
            0x8000_5000,
            0x8000_8000,
        );
        setup_queue(&mmio, desc, avail, used);
        let mut avail_idx = 0u16;
        let mut cmd = |req: &[u8]| -> Vec<u8> {
            dma.write_bytes(req_buf, req).unwrap();
            dma.write_bytes(resp_buf, &[0u8; 1100]).unwrap();
            write_desc(
                &dma,
                desc,
                0,
                req_buf,
                req.len() as u32,
                VIRTQ_DESC_F_NEXT,
                1,
            );
            write_desc(&dma, desc, 1, resp_buf, 1100, VIRTQ_DESC_F_WRITE, 0);
            avail_idx += 1;
            dma.write_u16(avail + 2, avail_idx).unwrap();
            dma.write_u16(avail + 4 + u64::from(avail_idx - 1) * 2, 0)
                .unwrap();
            mmio.write(0x050, AccessSize::Word, 0);
            let mut out = vec![0u8; 64];
            dma.read_bytes(resp_buf, &mut out).unwrap();
            out
        };

        // 1. GET_DISPLAY_INFO (0x0101)
        let mut get_info = [0u8; 24];
        get_info[0..4].copy_from_slice(&0x0101u32.to_le_bytes());
        let resp = cmd(&get_info);
        assert_eq!(u32::from_le_bytes(resp[0..4].try_into().unwrap()), 0x1101);
        assert_eq!(u32::from_le_bytes(resp[32..36].try_into().unwrap()), 32);
        assert_eq!(u32::from_le_bytes(resp[36..40].try_into().unwrap()), 16);

        // 2. RESOURCE_CREATE_2D (0x0102): res_id=1, format=1 (B8G8R8A8_UNORM), 32x16
        let mut create = [0u8; 40];
        create[0..4].copy_from_slice(&0x0102u32.to_le_bytes());
        create[24..28].copy_from_slice(&1u32.to_le_bytes());
        create[28..32].copy_from_slice(&1u32.to_le_bytes());
        create[32..36].copy_from_slice(&32u32.to_le_bytes());
        create[36..40].copy_from_slice(&16u32.to_le_bytes());
        assert_eq!(
            u32::from_le_bytes(cmd(&create)[0..4].try_into().unwrap()),
            0x1100
        );

        // 3. Populate guest backing with B8G8R8A8 pixel [B=0x33, G=0x22, R=0x11, A=0xff]
        let mut raw_pixels = Vec::with_capacity(32 * 16 * 4);
        for _ in 0..(32 * 16) {
            raw_pixels.extend_from_slice(&[0x33, 0x22, 0x11, 0xff]);
        }
        dma.write_bytes(backing_buf, &raw_pixels).unwrap();

        // 4. RESOURCE_ATTACH_BACKING (0x0107): res_id=1, nr_entries=1
        let mut attach = [0u8; 48];
        attach[0..4].copy_from_slice(&0x0107u32.to_le_bytes());
        attach[24..28].copy_from_slice(&1u32.to_le_bytes());
        attach[28..32].copy_from_slice(&1u32.to_le_bytes());
        attach[32..40].copy_from_slice(&backing_buf.to_le_bytes());
        attach[40..44].copy_from_slice(&(raw_pixels.len() as u32).to_le_bytes());
        assert_eq!(
            u32::from_le_bytes(cmd(&attach)[0..4].try_into().unwrap()),
            0x1100
        );

        // 5. SET_SCANOUT (0x0104): scanout_id=0, res_id=1
        let mut scanout = [0u8; 48];
        scanout[0..4].copy_from_slice(&0x0104u32.to_le_bytes());
        scanout[32..36].copy_from_slice(&32u32.to_le_bytes());
        scanout[36..40].copy_from_slice(&16u32.to_le_bytes());
        scanout[44..48].copy_from_slice(&1u32.to_le_bytes());
        assert_eq!(
            u32::from_le_bytes(cmd(&scanout)[0..4].try_into().unwrap()),
            0x1100
        );

        // 6. TRANSFER_TO_HOST_2D (0x0106) + RESOURCE_FLUSH (0x0105)
        let mut xfer = [0u8; 56];
        xfer[0..4].copy_from_slice(&0x0106u32.to_le_bytes());
        xfer[32..36].copy_from_slice(&32u32.to_le_bytes());
        xfer[36..40].copy_from_slice(&16u32.to_le_bytes());
        xfer[48..52].copy_from_slice(&1u32.to_le_bytes());
        assert_eq!(
            u32::from_le_bytes(cmd(&xfer)[0..4].try_into().unwrap()),
            0x1100
        );

        let mut flush = [0u8; 48];
        flush[0..4].copy_from_slice(&0x0105u32.to_le_bytes());
        flush[32..36].copy_from_slice(&32u32.to_le_bytes());
        flush[36..40].copy_from_slice(&16u32.to_le_bytes());
        flush[40..44].copy_from_slice(&1u32.to_le_bytes());
        assert_eq!(
            u32::from_le_bytes(cmd(&flush)[0..4].try_into().unwrap()),
            0x1100
        );

        let ppm = fb.to_ppm();
        assert!(ppm.starts_with(b"P6\n32 16\n255\n"));
        assert_eq!(&ppm[13..16], &[0x11, 0x22, 0x33]); // RGB converted from BGRA!

        let png = fb.to_png();
        assert_eq!(
            &png[0..8],
            &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]
        );
        assert!(png.windows(4).any(|w| w == b"IHDR"));
        assert!(png.windows(4).any(|w| w == b"IDAT"));
        assert!(png.windows(4).any(|w| w == b"IEND"));
    }

    #[test]
    fn virtio_input_and_vnc_server_roundtrip() {
        use std::io::{Read, Write};
        use std::net::TcpStream;

        let dma = test_dma(0x1_0000);
        let irq = Arc::new(TestIrq::default());
        let hub = InputHub::new();
        let mmio = VirtioMmio::new(Box::new(VirtioInput::new(hub.clone())), dma.clone(), irq);
        assert_eq!(mmio.read(0x008, AccessSize::Word), 18);

        // Query VIRTIO_INPUT_CFG_ID_NAME (select = 1, subsel = 0).
        mmio.write(0x100, AccessSize::Half, 0x0001);
        let name_len = mmio.read(0x102, AccessSize::Byte);
        assert!(name_len > 0);

        // Populate 2 writable 8-byte event buffers on queue 0 (`eventq`).
        let (desc, avail, used, ev0, ev1) = (
            0x8000_1000,
            0x8000_2000,
            0x8000_3000,
            0x8000_4000,
            0x8000_4010,
        );
        setup_queue(&mmio, desc, avail, used);
        write_desc(&dma, desc, 0, ev0, 8, VIRTQ_DESC_F_WRITE, 0);
        write_desc(&dma, desc, 1, ev1, 8, VIRTQ_DESC_F_WRITE, 0);
        dma.write_u16(avail + 2, 2).unwrap();
        dma.write_u16(avail + 4, 0).unwrap();
        dma.write_u16(avail + 6, 1).unwrap();

        // Start a live VNC server on an ephemeral port and connect a client.
        let fb = Arc::new(Framebuffer::new(16, 16));
        let mut vnc = VncServer::bind("127.0.0.1:0", fb, hub).unwrap();
        let addr = vnc.local_addr().to_owned();

        let client_handle = std::thread::spawn(move || {
            let mut client = TcpStream::connect(&addr).unwrap();
            let mut banner = [0u8; 12];
            client.read_exact(&mut banner).unwrap();
            assert_eq!(&banner, b"RFB 003.008\n");

            client.write_all(b"RFB 003.008\n").unwrap();
            let mut sec = [0u8; 2];
            client.read_exact(&mut sec).unwrap();
            assert_eq!(sec, [1, 1]);

            client.write_all(&[1]).unwrap();
            let mut sec_res = [0u8; 4];
            client.read_exact(&mut sec_res).unwrap();
            assert_eq!(sec_res, [0, 0, 0, 0]);

            client.write_all(&[1]).unwrap();
            let mut srv_init = [0u8; 30];
            client.read_exact(&mut srv_init).unwrap();
            assert_eq!(&srv_init[24..30], b"rivemu");

            // Request a FramebufferUpdate (type=3, incremental=0, x=0, y=0, w=16, h=16)
            client.write_all(&[3, 0, 0, 0, 0, 0, 0, 16, 0, 16]).unwrap();
            let mut fb_upd = vec![0u8; 16 + 16 * 16 * 4];
            client.read_exact(&mut fb_upd).unwrap();
            assert_eq!(&fb_upd[0..4], &[0, 0, 0, 1]);

            // Send a VNC KeyEvent (type=4, down=1, pad=0, keysym='a'=0x61).
            client.write_all(&[4, 1, 0, 0, 0, 0, 0, 0x61]).unwrap();
            client.flush().unwrap();
        });

        while !client_handle.is_finished() {
            vnc.poll();
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        client_handle.join().unwrap();
        for _ in 0..10 {
            vnc.poll();
            mmio.poll();
            if dma.read_u16(used + 2) == Some(2) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }

        // Verify the guest received EV_KEY (KEY_A = 30, down = 1) followed by EV_SYN.
        assert_eq!(dma.read_u16(used + 2), Some(2));
        assert_eq!(dma.read_u16(ev0), Some(1)); // EV_KEY
        assert_eq!(dma.read_u16(ev0 + 2), Some(30)); // KEY_A
        assert_eq!(dma.read_u32(ev0 + 4), Some(1)); // down = 1
        assert_eq!(dma.read_u16(ev1), Some(0)); // EV_SYN
    }

    #[test]
    fn virtio_vsock_connect_stream_rw_and_credit_update() {
        let dma = test_dma(0x10000);
        let irq = Arc::new(TestIrq::default());
        let endpoint = VsockHostEndpoint::new();
        let mmio = VirtioMmio::new(
            Box::new(VirtioVsock::new(DEFAULT_GUEST_CID, endpoint.clone())),
            dma.clone(),
            irq.clone(),
        );

        assert_eq!(mmio.read(0x008, AccessSize::Word), 19);
        assert_eq!(mmio.read(0x100, AccessSize::Double), DEFAULT_GUEST_CID);

        // Setup queue 0 (rx) and queue 1 (tx)
        let (rx_desc, rx_avail, rx_used) = (0x8000_1000, 0x8000_2000, 0x8000_3000);
        let (tx_desc, tx_avail, tx_used) = (0x8000_4000, 0x8000_5000, 0x8000_6000);
        setup_queue(&mmio, rx_desc, rx_avail, rx_used);
        mmio.write(0x030, AccessSize::Word, 1);
        mmio.write(0x038, AccessSize::Word, 16);
        mmio.write(0x080, AccessSize::Word, tx_desc);
        mmio.write(0x084, AccessSize::Word, tx_desc >> 32);
        mmio.write(0x090, AccessSize::Word, tx_avail);
        mmio.write(0x094, AccessSize::Word, tx_avail >> 32);
        mmio.write(0x0a0, AccessSize::Word, tx_used);
        mmio.write(0x0a4, AccessSize::Word, tx_used >> 32);
        mmio.write(0x044, AccessSize::Word, 1);

        // Provide two RX buffers at 0x8000_7000 and 0x8000_7200
        write_desc(&dma, rx_desc, 0, 0x8000_7000, 256, VIRTQ_DESC_F_WRITE, 0);
        write_desc(&dma, rx_desc, 1, 0x8000_7200, 256, VIRTQ_DESC_F_WRITE, 0);
        dma.write_u16(rx_avail + 2, 2).unwrap();
        dma.write_u16(rx_avail + 4, 0).unwrap();
        dma.write_u16(rx_avail + 6, 1).unwrap();

        // Guest sends REQUEST (op=1) followed by RW (op=5, payload="hello-vsock") on TX queue
        let req_hdr = VsockHeader {
            src_cid: DEFAULT_GUEST_CID,
            dst_cid: VMADDR_CID_HOST,
            src_port: 1024,
            dst_port: 9000,
            len: 0,
            r#type: VIRTIO_VSOCK_TYPE_STREAM,
            op: vsock_op::REQUEST,
            flags: 0,
            buf_alloc: 65536,
            fwd_cnt: 0,
        };
        let rw_hdr = VsockHeader {
            len: 11,
            op: vsock_op::RW,
            ..req_hdr
        };
        dma.write_bytes(0x8000_8000, &req_hdr.to_bytes());
        let mut rw_pkt = rw_hdr.to_bytes().to_vec();
        rw_pkt.extend_from_slice(b"hello-vsock");
        dma.write_bytes(0x8000_8100, &rw_pkt);

        write_desc(&dma, tx_desc, 0, 0x8000_8000, VSOCK_HDR_SIZE as u32, 0, 0);
        write_desc(&dma, tx_desc, 1, 0x8000_8100, rw_pkt.len() as u32, 0, 0);
        dma.write_u16(tx_avail + 2, 2).unwrap();
        dma.write_u16(tx_avail + 4, 0).unwrap();
        dma.write_u16(tx_avail + 6, 1).unwrap();
        mmio.write(0x050, AccessSize::Word, 1); // notify TX

        // Poll RX queue to deliver RESPONSE and CREDIT_UPDATE
        mmio.poll();
        assert_eq!(dma.read_u16(rx_used + 2), Some(2));

        let mut resp_buf = [0u8; VSOCK_HDR_SIZE];
        dma.read_bytes(0x8000_7000, &mut resp_buf);
        let resp = VsockHeader::from_bytes(&resp_buf).unwrap();
        assert_eq!(resp.op, vsock_op::RESPONSE);
        assert_eq!(resp.src_cid, VMADDR_CID_HOST);
        assert_eq!(resp.dst_cid, DEFAULT_GUEST_CID);
        assert_eq!(resp.dst_port, 1024);

        let mut credit_buf = [0u8; VSOCK_HDR_SIZE];
        dma.read_bytes(0x8000_7200, &mut credit_buf);
        let credit = VsockHeader::from_bytes(&credit_buf).unwrap();
        assert_eq!(credit.op, vsock_op::CREDIT_UPDATE);
        assert_eq!(credit.fwd_cnt, 11);

        let payloads = endpoint.take_tx_payloads();
        assert_eq!(payloads, vec![(1024, 9000, b"hello-vsock".to_vec())]);
    }

    #[test]
    fn pcie_nvme_identify_create_io_queues_write_read_and_snapshot() {
        let dma = test_dma(0x10000);
        let irq: Arc<dyn IrqLine> = Arc::new(TestIrq::default());
        let irqs: [Arc<dyn IrqLine>; 4] = [irq.clone(), irq.clone(), irq.clone(), irq.clone()];
        let pci = PciBus::new(dma.clone(), irqs);
        let disk = MemoryBlock::new(64 * 1024, false);
        pci.add_nvme_device(disk, "NVME-TEST-01");

        let ecam = pci.ecam_window();
        let mmio = pci.mmio_window();

        // Slot 1 ECAM offset = 1 << 15: Vendor 0x1b36, Device 0x0010, Class 0x010802
        let slot1_ecam = 1u64 << 15;
        assert_eq!(ecam.read(slot1_ecam, AccessSize::Word), 0x0010_1b36);
        assert_eq!(ecam.read(slot1_ecam + 0x08, AccessSize::Word) >> 8, 0x010802);

        // Slot 1 BAR0 is at offset 0x1_0000 in PCI MMIO window (`0x4001_0000`)
        let bar0 = 0x1_0000u64;
        assert_eq!(mmio.read(bar0 + 0x08, AccessSize::Word), 0x0001_0400); // VS = 1.4.0

        // Configure Admin SQ @ 0x8000_1000, Admin CQ @ 0x8000_2000, AQA = (15 << 16) | 15, CC.EN = 1
        let (asq, acq) = (0x8000_1000u64, 0x8000_2000u64);
        mmio.write(bar0 + 0x24, AccessSize::Word, (15 << 16) | 15);
        mmio.write(bar0 + 0x28, AccessSize::Double, asq);
        mmio.write(bar0 + 0x30, AccessSize::Double, acq);
        mmio.write(bar0 + 0x14, AccessSize::Word, 1); // CC.EN = 1
        assert_eq!(mmio.read(bar0 + 0x1c, AccessSize::Word) & 1, 1); // CSTS.RDY = 1

        // Helper to submit a 64-byte SQE on queue `qid` at slot `tail`
        let submit = |qid: u64, sq_base: u64, idx: u64, sqe: &[u8; 64]| {
            dma.write_bytes(sq_base + idx * 64, sqe);
            mmio.write(bar0 + 0x1000 + (2 * qid) * 4, AccessSize::Word, idx + 1);
        };

        // 1. Admin Identify Controller (opcode 0x06, CNS=1) -> PRP1 = 0x8000_3000
        let mut sqe = [0u8; 64];
        sqe[0] = 0x06;
        sqe[2..4].copy_from_slice(&1u16.to_le_bytes()); // CID = 1
        sqe[24..32].copy_from_slice(&0x8000_3000u64.to_le_bytes()); // PRP1
        sqe[40..44].copy_from_slice(&1u32.to_le_bytes()); // CDW10: CNS = 1
        submit(0, asq, 0, &sqe);

        let mut sn = [0u8; 12];
        dma.read_bytes(0x8000_3004, &mut sn);
        assert_eq!(&sn, b"NVME-TEST-01");
        assert_eq!(dma.read_u16(acq + 12), Some(1)); // CQE0 CID = 1
        assert_eq!(dma.read_u16(acq + 14), Some(1)); // status=0, phase=1

        // 2. Create I/O CQ 1 @ 0x8000_5000 (opcode 0x05) and I/O SQ 1 @ 0x8000_4000 (opcode 0x01)
        let (iosq, iocq) = (0x8000_4000u64, 0x8000_5000u64);
        let mut create_cq = [0u8; 64];
        create_cq[0] = 0x05;
        create_cq[24..32].copy_from_slice(&iocq.to_le_bytes());
        create_cq[40..44].copy_from_slice(&((15u32 << 16) | 1).to_le_bytes()); // QID=1, QSIZE=15
        create_cq[44..48].copy_from_slice(&3u32.to_le_bytes()); // PC=1, IEN=1
        submit(0, asq, 1, &create_cq);

        let mut create_sq = [0u8; 64];
        create_sq[0] = 0x01;
        create_sq[24..32].copy_from_slice(&iosq.to_le_bytes());
        create_sq[40..44].copy_from_slice(&((15u32 << 16) | 1).to_le_bytes()); // QID=1, QSIZE=15
        create_sq[44..48].copy_from_slice(&((1u32 << 16) | 1).to_le_bytes()); // CQID=1, PC=1
        submit(0, asq, 2, &create_sq);

        // Snapshot PCI bus state, reset NVMe controller, and restore snapshot!
        let (v_snap, n_snap) = pci.snapshot();
        mmio.write(bar0 + 0x14, AccessSize::Word, 0); // CC.EN = 0 (reset)
        assert_eq!(mmio.read(bar0 + 0x1c, AccessSize::Word) & 1, 0);
        pci.restore(v_snap, n_snap);
        assert_eq!(mmio.read(bar0 + 0x1c, AccessSize::Word) & 1, 1);

        // 3. I/O Write (opcode 0x01) 1 sector (512B) at LBA 4 from 0x8000_6000, then Read (opcode 0x02) into 0x8000_7000
        let pattern = [0xa5u8; 512];
        dma.write_bytes(0x8000_6000, &pattern);
        let mut io_write = [0u8; 64];
        io_write[0] = 0x01;
        io_write[2..4].copy_from_slice(&0x42u16.to_le_bytes());
        io_write[4..8].copy_from_slice(&1u32.to_le_bytes()); // NSID = 1
        io_write[24..32].copy_from_slice(&0x8000_6000u64.to_le_bytes()); // PRP1
        io_write[40..44].copy_from_slice(&4u32.to_le_bytes()); // SLBA = 4
        submit(1, iosq, 0, &io_write);

        let mut io_read = [0u8; 64];
        io_read[0] = 0x02;
        io_read[2..4].copy_from_slice(&0x43u16.to_le_bytes());
        io_read[4..8].copy_from_slice(&1u32.to_le_bytes()); // NSID = 1
        io_read[24..32].copy_from_slice(&0x8000_7000u64.to_le_bytes()); // PRP1
        io_read[40..44].copy_from_slice(&4u32.to_le_bytes()); // SLBA = 4
        submit(1, iosq, 1, &io_read);

        let mut readback = [0u8; 512];
        dma.read_bytes(0x8000_7000, &mut readback);
        assert_eq!(readback, pattern);
        assert_eq!(dma.read_u16(iocq + 16 + 12), Some(0x43)); // second I/O CQE CID
    }
}
