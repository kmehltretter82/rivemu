//! `virtio-console` (`virtio-serial-device`, `DeviceID = 3`).

use std::sync::Arc;

use rivemu_backends::Chardev;
use rivemu_core::AccessSize;

use crate::VirtioDevice;
use crate::blk::read_cfg_slice;
use crate::queue::{DmaRam, VirtQueue};

const VIRTIO_CONSOLE_F_SIZE: u64 = 1 << 0;
const VIRTIO_CONSOLE_F_EMERG_WRITE: u64 = 1 << 2;

/// Single-port `virtio-console` device backed by a [`Chardev`].
#[derive(Debug)]
pub struct VirtioConsole {
    chardev: Arc<dyn Chardev>,
    rx_pending: Vec<u8>,
}

impl VirtioConsole {
    /// Creates a new `virtio-console` connected to `chardev`.
    pub fn new(chardev: Arc<dyn Chardev>) -> Self {
        Self {
            chardev,
            rx_pending: Vec::new(),
        }
    }

    fn config_bytes() -> [u8; 12] {
        let mut cfg = [0u8; 12];
        cfg[0..2].copy_from_slice(&80u16.to_le_bytes()); // cols
        cfg[2..4].copy_from_slice(&25u16.to_le_bytes()); // rows
        cfg[4..8].copy_from_slice(&1u32.to_le_bytes()); // max_nr_ports
        cfg
    }

    fn deliver_rx(&mut self, rxq: &mut VirtQueue, dma: &DmaRam, event_idx: bool) -> u32 {
        let incoming = self.chardev.read_input();
        if !incoming.is_empty() {
            self.rx_pending.extend_from_slice(&incoming);
        }
        let mut delivered = 0;
        while !self.rx_pending.is_empty() && rxq.has_available(dma) {
            let Some(chain) = rxq.pop(dma) else {
                break;
            };
            let cap = chain.writable_len();
            let n = cap.min(self.rx_pending.len());
            let written = chain.write_all(dma, &self.rx_pending[..n]).unwrap_or(0);
            self.rx_pending.drain(..n);
            rxq.push_used(dma, chain.head, written, event_idx);
            delivered += 1;
        }
        delivered
    }
}

impl VirtioDevice for VirtioConsole {
    fn device_id(&self) -> u32 {
        3
    }

    fn num_queues(&self) -> usize {
        2 // 0 = receiveq0, 1 = transmitq0
    }

    fn device_features(&self) -> u64 {
        VIRTIO_CONSOLE_F_SIZE | VIRTIO_CONSOLE_F_EMERG_WRITE
    }

    fn read_config(&self, offset: u64, size: AccessSize) -> u64 {
        read_cfg_slice(&Self::config_bytes(), offset, size)
    }

    fn write_config(&mut self, offset: u64, _size: AccessSize, value: u64) {
        // Emergency write register at offset 8.
        if offset == 8 {
            self.chardev.write_output(&[value as u8]);
        }
    }

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
            if let Some(bytes) = chain.read_all(dma) {
                self.chardev.write_output(&bytes);
            }
            queue.push_used(dma, chain.head, 0, event_idx);
            sent += 1;
        }
        sent
    }

    fn poll(&mut self, queues: &mut [VirtQueue], dma: &DmaRam, event_idx: bool) -> u32 {
        if let Some(rxq) = queues.first_mut() {
            self.deliver_rx(rxq, dma, event_idx)
        } else {
            0
        }
    }
}
