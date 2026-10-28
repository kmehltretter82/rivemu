//! `virtio-net` device (`DeviceID = 1`).

use std::sync::Arc;

use rivemu_backends::NetBackend;
use rivemu_core::AccessSize;

use crate::VirtioDevice;
use crate::blk::read_cfg_slice;
use crate::queue::{DmaRam, VirtQueue};

const VIRTIO_NET_F_MAC: u64 = 1 << 5;
const VIRTIO_NET_F_STATUS: u64 = 1 << 16;
const VIRTIO_NET_S_LINK_UP: u16 = 1;

/// Size of `virtio_net_hdr_v1` in bytes.
const NET_HDR_LEN: usize = 12;

/// A `virtio-net-device` backed by a [`NetBackend`].
#[derive(Debug)]
pub struct VirtioNet {
    backend: Arc<dyn NetBackend>,
}

impl VirtioNet {
    /// Creates a new `virtio-net` device.
    pub fn new(backend: Arc<dyn NetBackend>) -> Self {
        Self { backend }
    }

    fn config_bytes(&self) -> [u8; 8] {
        let mut cfg = [0u8; 8];
        cfg[0..6].copy_from_slice(&self.backend.mac());
        cfg[6..8].copy_from_slice(&VIRTIO_NET_S_LINK_UP.to_le_bytes());
        cfg
    }

    fn deliver_rx(&mut self, rxq: &mut VirtQueue, dma: &DmaRam, event_idx: bool) -> u32 {
        let mut delivered = 0;
        while rxq.has_available(dma) {
            let Some(frame) = self.backend.recv_packet() else {
                break;
            };
            let Some(chain) = rxq.pop(dma) else {
                break;
            };
            let mut buf = vec![0u8; NET_HDR_LEN + frame.len()];
            // num_buffers = 1 at offset 10..12
            buf[10..12].copy_from_slice(&1u16.to_le_bytes());
            buf[NET_HDR_LEN..].copy_from_slice(&frame);
            let written = chain.write_all(dma, &buf).unwrap_or(0);
            rxq.push_used(dma, chain.head, written, event_idx);
            delivered += 1;
        }
        delivered
    }
}

impl VirtioDevice for VirtioNet {
    fn device_id(&self) -> u32 {
        1
    }

    fn num_queues(&self) -> usize {
        2 // 0 = RX, 1 = TX
    }

    fn device_features(&self) -> u64 {
        VIRTIO_NET_F_MAC | VIRTIO_NET_F_STATUS
    }

    fn read_config(&self, offset: u64, size: AccessSize) -> u64 {
        read_cfg_slice(&self.config_bytes(), offset, size)
    }

    fn write_config(&mut self, _offset: u64, _size: AccessSize, _value: u64) {}

    fn process_queue(
        &mut self,
        queue_idx: usize,
        queue: &mut VirtQueue,
        dma: &DmaRam,
        event_idx: bool,
    ) -> u32 {
        if queue_idx == 0 {
            return self.deliver_rx(queue, dma, event_idx);
        }
        let mut sent = 0;
        while let Some(chain) = queue.pop(dma) {
            if let Some(pkt) = chain.read_all(dma) {
                if pkt.len() > NET_HDR_LEN {
                    self.backend.send_packet(&pkt[NET_HDR_LEN..]);
                }
            }
            queue.push_used(dma, chain.head, 0, event_idx);
            sent += 1;
        }
        sent
    }

    fn poll(&mut self, queues: &mut [VirtQueue], dma: &DmaRam, event_idx: bool) -> u32 {
        self.backend.poll();
        if let Some(rxq) = queues.first_mut() {
            self.deliver_rx(rxq, dma, event_idx)
        } else {
            0
        }
    }
}
