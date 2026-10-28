//! `virtio-blk` device (`DeviceID = 2`).

use std::sync::Arc;

use rivemu_backends::{BlockBackend, SECTOR_SIZE};
use rivemu_core::AccessSize;

use crate::VirtioDevice;
use crate::queue::{DmaRam, VirtQueue};

const VIRTIO_BLK_F_SIZE_MAX: u64 = 1 << 1;
const VIRTIO_BLK_F_SEG_MAX: u64 = 1 << 2;
const VIRTIO_BLK_F_RO: u64 = 1 << 5;
const VIRTIO_BLK_F_BLK_SIZE: u64 = 1 << 6;
const VIRTIO_BLK_F_FLUSH: u64 = 1 << 9;

const VIRTIO_BLK_T_IN: u32 = 0;
const VIRTIO_BLK_T_OUT: u32 = 1;
const VIRTIO_BLK_T_FLUSH: u32 = 4;
const VIRTIO_BLK_T_GET_ID: u32 = 8;

const VIRTIO_BLK_S_OK: u8 = 0;
const VIRTIO_BLK_S_IOERR: u8 = 1;
const VIRTIO_BLK_S_UNSUPP: u8 = 2;

/// A `virtio-blk-device` backed by a [`BlockBackend`].
#[derive(Debug)]
pub struct VirtioBlk {
    backend: Arc<dyn BlockBackend>,
    id: [u8; 20],
}

impl VirtioBlk {
    /// Creates a new `virtio-blk` device.
    pub fn new(backend: Arc<dyn BlockBackend>, serial: &str) -> Self {
        let mut id = [0u8; 20];
        let bytes = serial.as_bytes();
        let n = bytes.len().min(id.len());
        id[..n].copy_from_slice(&bytes[..n]);
        Self { backend, id }
    }

    fn config_bytes(&self) -> [u8; 24] {
        let mut cfg = [0u8; 24];
        cfg[0..8].copy_from_slice(&self.backend.capacity_sectors().to_le_bytes());
        cfg[8..12].copy_from_slice(&(1024 * 1024u32).to_le_bytes()); // size_max
        cfg[12..16].copy_from_slice(&128u32.to_le_bytes()); // seg_max
        cfg[20..24].copy_from_slice(&(SECTOR_SIZE as u32).to_le_bytes()); // blk_size
        cfg
    }
}

impl VirtioDevice for VirtioBlk {
    fn device_id(&self) -> u32 {
        2
    }

    fn num_queues(&self) -> usize {
        1
    }

    fn device_features(&self) -> u64 {
        let mut f = VIRTIO_BLK_F_SIZE_MAX
            | VIRTIO_BLK_F_SEG_MAX
            | VIRTIO_BLK_F_BLK_SIZE
            | VIRTIO_BLK_F_FLUSH;
        if self.backend.is_read_only() {
            f |= VIRTIO_BLK_F_RO;
        }
        f
    }

    fn read_config(&self, offset: u64, size: AccessSize) -> u64 {
        read_cfg_slice(&self.config_bytes(), offset, size)
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
            let Some(req_bytes) = chain.read_all(dma) else {
                continue;
            };
            let wlen = chain.writable_len();
            if req_bytes.len() < 16 || wlen < 1 {
                continue;
            }
            let req_type = u32::from_le_bytes(req_bytes[0..4].try_into().unwrap());
            let sector = u64::from_le_bytes(req_bytes[8..16].try_into().unwrap());
            let data_out = &req_bytes[16..];
            let data_in_len = wlen - 1;

            let mut resp = vec![0u8; wlen];
            let status = match req_type {
                VIRTIO_BLK_T_IN => {
                    match self
                        .backend
                        .read_at(sector * SECTOR_SIZE, &mut resp[..data_in_len])
                    {
                        Ok(()) => VIRTIO_BLK_S_OK,
                        Err(_) => VIRTIO_BLK_S_IOERR,
                    }
                }
                VIRTIO_BLK_T_OUT => {
                    if self.backend.is_read_only() {
                        VIRTIO_BLK_S_IOERR
                    } else {
                        match self.backend.write_at(sector * SECTOR_SIZE, data_out) {
                            Ok(()) => VIRTIO_BLK_S_OK,
                            Err(_) => VIRTIO_BLK_S_IOERR,
                        }
                    }
                }
                VIRTIO_BLK_T_FLUSH => match self.backend.flush() {
                    Ok(()) => VIRTIO_BLK_S_OK,
                    Err(_) => VIRTIO_BLK_S_IOERR,
                },
                VIRTIO_BLK_T_GET_ID => {
                    let n = data_in_len.min(self.id.len());
                    resp[..n].copy_from_slice(&self.id[..n]);
                    VIRTIO_BLK_S_OK
                }
                _ => VIRTIO_BLK_S_UNSUPP,
            };
            resp[data_in_len] = status;
            let written = chain.write_all(dma, &resp).unwrap_or(0);
            queue.push_used(dma, chain.head, written, event_idx);
            completed += 1;
        }
        completed
    }
}

pub(crate) fn read_cfg_slice(cfg: &[u8], offset: u64, size: AccessSize) -> u64 {
    let mut v = 0u64;
    for i in 0..size.bytes() {
        let idx = offset.wrapping_add(i) as usize;
        let b = cfg.get(idx).copied().unwrap_or(0);
        v |= u64::from(b) << (8 * i);
    }
    v
}
