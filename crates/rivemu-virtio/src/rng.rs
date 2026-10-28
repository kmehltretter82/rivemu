//! `virtio-rng` entropy device (`DeviceID = 4`).

use rivemu_core::AccessSize;

use crate::VirtioDevice;
use crate::queue::{DmaRam, VirtQueue};

/// A `virtio-rng-device` providing entropy to the guest kernel.
#[derive(Debug)]
pub struct VirtioRng {
    state: u64,
}

impl Default for VirtioRng {
    fn default() -> Self {
        Self::new(0x853c_49e6_748f_ea9b)
    }
}

impl VirtioRng {
    /// Creates a new `virtio-rng` device with the given initial seed.
    pub fn new(seed: u64) -> Self {
        Self {
            state: if seed == 0 { 1 } else { seed },
        }
    }

    fn fill_bytes(&mut self, buf: &mut [u8]) {
        for chunk in buf.chunks_mut(8) {
            // xorshift64*
            let mut x = self.state;
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            self.state = x;
            let bytes = x.wrapping_mul(0x2545_f491_4f6c_dd1d).to_le_bytes();
            chunk.copy_from_slice(&bytes[..chunk.len()]);
        }
    }
}

impl VirtioDevice for VirtioRng {
    fn device_id(&self) -> u32 {
        4
    }

    fn num_queues(&self) -> usize {
        1
    }

    fn device_features(&self) -> u64 {
        0
    }

    fn read_config(&self, _offset: u64, _size: AccessSize) -> u64 {
        0
    }

    fn write_config(&mut self, _offset: u64, _size: AccessSize, _value: u64) {}

    fn process_queue(
        &mut self,
        _queue_idx: usize,
        queue: &mut VirtQueue,
        dma: &DmaRam,
        event_idx: bool,
    ) -> u32 {
        let mut completed = 0;
        while let Some(chain) = queue.pop(dma) {
            let len = chain.writable_len();
            let mut buf = vec![0u8; len];
            self.fill_bytes(&mut buf);
            let written = chain.write_all(dma, &buf).unwrap_or(0);
            queue.push_used(dma, chain.head, written, event_idx);
            completed += 1;
        }
        completed
    }
}
