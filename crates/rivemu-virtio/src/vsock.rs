//! `virtio-vsock` (`vhost-vsock-device` / `virtio-vsock-pci`, `DeviceID = 19`).
//!
//! Implements the VirtIO Socket device (`VIRTIO_ID_VSOCK = 19`) with:
//! - 64-bit `guest_cid` configuration space (`0x00..0x08`)
//! - 3 virtqueues: `0 = rx` (host -> guest), `1 = tx` (guest -> host), `2 = event`
//! - 44-byte `virtio_vsock_hdr` stream framing (`VIRTIO_VSOCK_TYPE_STREAM = 1`)
//! - Built-in host CID (`VMADDR_CID_HOST = 2`) loopback / echo & host queue channel.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use rivemu_core::AccessSize;

use crate::VirtioDevice;
use crate::blk::read_cfg_slice;
use crate::queue::{DmaRam, VirtQueue};

/// Host context ID (`VMADDR_CID_HOST`).
pub const VMADDR_CID_HOST: u64 = 2;
/// Default guest context ID (`3`).
pub const DEFAULT_GUEST_CID: u64 = 3;

/// Size of `struct virtio_vsock_hdr` in bytes.
pub const VSOCK_HDR_SIZE: usize = 44;

/// `VIRTIO_VSOCK_TYPE_STREAM`.
pub const VIRTIO_VSOCK_TYPE_STREAM: u16 = 1;

/// `virtio_vsock_op` constants.
pub mod vsock_op {
    /// Invalid op.
    pub const INVALID: u16 = 0;
    /// Connect request.
    pub const REQUEST: u16 = 1;
    /// Connect response.
    pub const RESPONSE: u16 = 2;
    /// Connection reset.
    pub const RST: u16 = 3;
    /// Connection shutdown.
    pub const SHUTDOWN: u16 = 4;
    /// Data read/write payload.
    pub const RW: u16 = 5;
    /// Credit update.
    pub const CREDIT_UPDATE: u16 = 6;
    /// Credit request.
    pub const CREDIT_REQUEST: u16 = 7;
}

const DEFAULT_BUF_ALLOC: u32 = 256 * 1024;

/// Parsed 44-byte `virtio_vsock_hdr`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VsockHeader {
    /// Source CID.
    pub src_cid: u64,
    /// Destination CID.
    pub dst_cid: u64,
    /// Source port.
    pub src_port: u32,
    /// Destination port.
    pub dst_port: u32,
    /// Payload length in bytes.
    pub len: u32,
    /// Socket type (`1 = STREAM`).
    pub r#type: u16,
    /// Operation code (`vsock_op::*`).
    pub op: u16,
    /// Flags.
    pub flags: u32,
    /// Peer buffer allocation advertisement.
    pub buf_alloc: u32,
    /// Forwarded byte count for flow control.
    pub fwd_cnt: u32,
}

impl VsockHeader {
    /// Decodes a 44-byte little-endian `virtio_vsock_hdr`.
    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        if b.len() < VSOCK_HDR_SIZE {
            return None;
        }
        Some(Self {
            src_cid: u64::from_le_bytes(b[0..8].try_into().ok()?),
            dst_cid: u64::from_le_bytes(b[8..16].try_into().ok()?),
            src_port: u32::from_le_bytes(b[16..20].try_into().ok()?),
            dst_port: u32::from_le_bytes(b[20..24].try_into().ok()?),
            len: u32::from_le_bytes(b[24..28].try_into().ok()?),
            r#type: u16::from_le_bytes(b[28..30].try_into().ok()?),
            op: u16::from_le_bytes(b[30..32].try_into().ok()?),
            flags: u32::from_le_bytes(b[32..36].try_into().ok()?),
            buf_alloc: u32::from_le_bytes(b[36..40].try_into().ok()?),
            fwd_cnt: u32::from_le_bytes(b[40..44].try_into().ok()?),
        })
    }

    /// Encodes this header into a 44-byte little-endian array.
    pub fn to_bytes(self) -> [u8; VSOCK_HDR_SIZE] {
        let mut out = [0u8; VSOCK_HDR_SIZE];
        out[0..8].copy_from_slice(&self.src_cid.to_le_bytes());
        out[8..16].copy_from_slice(&self.dst_cid.to_le_bytes());
        out[16..20].copy_from_slice(&self.src_port.to_le_bytes());
        out[20..24].copy_from_slice(&self.dst_port.to_le_bytes());
        out[24..28].copy_from_slice(&self.len.to_le_bytes());
        out[28..30].copy_from_slice(&self.r#type.to_le_bytes());
        out[30..32].copy_from_slice(&self.op.to_le_bytes());
        out[32..36].copy_from_slice(&self.flags.to_le_bytes());
        out[36..40].copy_from_slice(&self.buf_alloc.to_le_bytes());
        out[40..44].copy_from_slice(&self.fwd_cnt.to_le_bytes());
        out
    }
}

/// Shared host-side endpoint for inspecting and injecting `virtio-vsock` packets.
#[derive(Debug, Default)]
pub struct VsockHostEndpoint {
    /// Packets queued to be delivered to the guest RX queue (`hdr || payload`).
    rx_packets: Mutex<VecDeque<Vec<u8>>>,
    /// Payloads received from the guest TX queue (`(src_port, dst_port, payload)`).
    tx_log: Mutex<Vec<(u32, u32, Vec<u8>)>>,
}

impl VsockHostEndpoint {
    /// Creates a new host endpoint.
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Enqueues a raw vsock packet (`header + payload`) for delivery to the guest RX virtqueue.
    pub fn push_rx_packet(&self, hdr: VsockHeader, payload: &[u8]) {
        let mut pkt = Vec::with_capacity(VSOCK_HDR_SIZE + payload.len());
        let mut h = hdr;
        h.len = payload.len() as u32;
        pkt.extend_from_slice(&h.to_bytes());
        pkt.extend_from_slice(payload);
        self.rx_packets.lock().unwrap().push_back(pkt);
    }

    /// Takes all `(src_port, dst_port, payload)` data packets sent by the guest.
    pub fn take_tx_payloads(&self) -> Vec<(u32, u32, Vec<u8>)> {
        std::mem::take(&mut *self.tx_log.lock().unwrap())
    }
}

/// A `virtio-vsock` device (`DeviceID = 19`).
#[derive(Debug)]
pub struct VirtioVsock {
    guest_cid: u64,
    fwd_cnt: u32,
    endpoint: Arc<VsockHostEndpoint>,
}

impl VirtioVsock {
    /// Creates a new `virtio-vsock` device with the given `guest_cid` (e.g. `3`).
    pub fn new(guest_cid: u64, endpoint: Arc<VsockHostEndpoint>) -> Self {
        Self {
            guest_cid,
            fwd_cnt: 0,
            endpoint,
        }
    }

    fn config_bytes(&self) -> [u8; 8] {
        self.guest_cid.to_le_bytes()
    }

    fn deliver_rx(&mut self, rxq: &mut VirtQueue, dma: &DmaRam, event_idx: bool) -> u32 {
        let mut delivered = 0;
        let mut rx_packets = self.endpoint.rx_packets.lock().unwrap();
        while !rx_packets.is_empty() && rxq.has_available(dma) {
            let Some(chain) = rxq.pop(dma) else {
                break;
            };
            let pkt = rx_packets.pop_front().unwrap();
            let written = chain.write_all(dma, &pkt).unwrap_or(0);
            rxq.push_used(dma, chain.head, written, event_idx);
            delivered += 1;
        }
        delivered
    }

    fn handle_tx(&mut self, txq: &mut VirtQueue, dma: &DmaRam, event_idx: bool) -> u32 {
        let mut completed = 0;
        while let Some(chain) = txq.pop(dma) {
            if let Some((bytes, hdr)) = chain
                .read_all(dma)
                .and_then(|b| VsockHeader::from_bytes(&b).map(|h| (b, h)))
            {
                let payload = bytes
                    .get(VSOCK_HDR_SIZE..)
                    .unwrap_or(&[])
                    .get(..(hdr.len as usize).min(bytes.len().saturating_sub(VSOCK_HDR_SIZE)))
                    .unwrap_or(&[]);
                match hdr.op {
                    vsock_op::REQUEST => {
                        let reply = VsockHeader {
                            src_cid: VMADDR_CID_HOST,
                            dst_cid: self.guest_cid,
                            src_port: hdr.dst_port,
                            dst_port: hdr.src_port,
                            len: 0,
                            r#type: VIRTIO_VSOCK_TYPE_STREAM,
                            op: vsock_op::RESPONSE,
                            flags: 0,
                            buf_alloc: DEFAULT_BUF_ALLOC,
                            fwd_cnt: self.fwd_cnt,
                        };
                        self.endpoint.push_rx_packet(reply, &[]);
                    }
                    vsock_op::RW => {
                        self.fwd_cnt = self.fwd_cnt.wrapping_add(payload.len() as u32);
                        self.endpoint.tx_log.lock().unwrap().push((
                            hdr.src_port,
                            hdr.dst_port,
                            payload.to_vec(),
                        ));
                        let credit = VsockHeader {
                            src_cid: VMADDR_CID_HOST,
                            dst_cid: self.guest_cid,
                            src_port: hdr.dst_port,
                            dst_port: hdr.src_port,
                            len: 0,
                            r#type: VIRTIO_VSOCK_TYPE_STREAM,
                            op: vsock_op::CREDIT_UPDATE,
                            flags: 0,
                            buf_alloc: DEFAULT_BUF_ALLOC,
                            fwd_cnt: self.fwd_cnt,
                        };
                        self.endpoint.push_rx_packet(credit, &[]);
                    }
                    vsock_op::CREDIT_REQUEST => {
                        let credit = VsockHeader {
                            src_cid: VMADDR_CID_HOST,
                            dst_cid: self.guest_cid,
                            src_port: hdr.dst_port,
                            dst_port: hdr.src_port,
                            len: 0,
                            r#type: VIRTIO_VSOCK_TYPE_STREAM,
                            op: vsock_op::CREDIT_UPDATE,
                            flags: 0,
                            buf_alloc: DEFAULT_BUF_ALLOC,
                            fwd_cnt: self.fwd_cnt,
                        };
                        self.endpoint.push_rx_packet(credit, &[]);
                    }
                    vsock_op::SHUTDOWN | vsock_op::RST => {
                        let rst = VsockHeader {
                            src_cid: VMADDR_CID_HOST,
                            dst_cid: self.guest_cid,
                            src_port: hdr.dst_port,
                            dst_port: hdr.src_port,
                            len: 0,
                            r#type: VIRTIO_VSOCK_TYPE_STREAM,
                            op: vsock_op::RST,
                            flags: 0,
                            buf_alloc: DEFAULT_BUF_ALLOC,
                            fwd_cnt: self.fwd_cnt,
                        };
                        self.endpoint.push_rx_packet(rst, &[]);
                    }
                    _ => {}
                }
            }
            txq.push_used(dma, chain.head, 0, event_idx);
            completed += 1;
        }
        completed
    }
}

impl VirtioDevice for VirtioVsock {
    fn device_id(&self) -> u32 {
        19
    }

    fn num_queues(&self) -> usize {
        3 // 0 = rx, 1 = tx, 2 = event
    }

    fn device_features(&self) -> u64 {
        0
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
        match queue_idx {
            0 => self.deliver_rx(queue, dma, event_idx),
            1 => self.handle_tx(queue, dma, event_idx),
            _ => 0,
        }
    }

    fn poll(&mut self, queues: &mut [VirtQueue], dma: &DmaRam, event_idx: bool) -> u32 {
        if let Some(rxq) = queues.first_mut() {
            self.deliver_rx(rxq, dma, event_idx)
        } else {
            0
        }
    }
}
