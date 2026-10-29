//! Split virtqueue implementation for VirtIO v1 (`virtio-mmio` v2).

use std::sync::Arc;

use rivemu_core::{AccessSize, GuestAddr};
use rivemu_mem::{Ram, ReservationInvalidator};

/// Maximum descriptors per virtqueue.
pub const QUEUE_MAX_SIZE: u32 = 256;

/// Descriptor continues via `next`.
pub const VIRTQ_DESC_F_NEXT: u16 = 1;
/// Buffer is device-writable (otherwise device-readable).
pub const VIRTQ_DESC_F_WRITE: u16 = 2;
/// Buffer contains a table of indirect descriptors.
pub const VIRTQ_DESC_F_INDIRECT: u16 = 4;

/// Guest RAM view at a fixed physical base address for VirtIO DMA.
#[derive(Clone, Debug)]
pub struct DmaRam {
    base: GuestAddr,
    ram: Arc<Ram>,
    reservations: ReservationInvalidator,
}

impl DmaRam {
    /// Creates a DMA view for `ram` mapped at physical `base`.
    pub fn new(base: GuestAddr, ram: Arc<Ram>, reservations: ReservationInvalidator) -> Self {
        Self {
            base,
            ram,
            reservations,
        }
    }

    #[inline]
    pub fn read_u16(&self, pa: GuestAddr) -> Option<u16> {
        self.ram
            .read(pa.checked_sub(self.base)?, AccessSize::Half)
            .map(|v| v as u16)
    }

    #[inline]
    pub fn read_u32(&self, pa: GuestAddr) -> Option<u32> {
        self.ram
            .read(pa.checked_sub(self.base)?, AccessSize::Word)
            .map(|v| v as u32)
    }

    #[inline]
    pub fn read_u64(&self, pa: GuestAddr) -> Option<u64> {
        self.ram
            .read(pa.checked_sub(self.base)?, AccessSize::Double)
    }

    #[inline]
    pub fn write_u16(&self, pa: GuestAddr, v: u16) -> Option<()> {
        self.ram
            .write(pa.checked_sub(self.base)?, AccessSize::Half, u64::from(v))?;
        self.reservations
            .invalidate_range(pa, AccessSize::Half.bytes());
        Some(())
    }

    #[inline]
    pub fn write_u32(&self, pa: GuestAddr, v: u32) -> Option<()> {
        self.ram
            .write(pa.checked_sub(self.base)?, AccessSize::Word, u64::from(v))?;
        self.reservations
            .invalidate_range(pa, AccessSize::Word.bytes());
        Some(())
    }

    #[inline]
    pub fn write_u64(&self, pa: GuestAddr, v: u64) -> Option<()> {
        self.ram
            .write(pa.checked_sub(self.base)?, AccessSize::Double, v)?;
        self.reservations
            .invalidate_range(pa, AccessSize::Double.bytes());
        Some(())
    }

    pub fn read_bytes(&self, pa: GuestAddr, buf: &mut [u8]) -> Option<()> {
        self.ram.read_bytes(pa.checked_sub(self.base)?, buf)
    }

    pub fn write_bytes(&self, pa: GuestAddr, data: &[u8]) -> Option<()> {
        self.ram.write_bytes(pa.checked_sub(self.base)?, data)?;
        self.reservations.invalidate_range(pa, data.len() as u64);
        Some(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rivemu_mem::AddressSpace;

    const BASE: GuestAddr = 0x8000_0000;

    fn dma_space(len: u64) -> (AddressSpace, DmaRam) {
        let ram = Arc::new(Ram::new(len).unwrap());
        let mut bus = AddressSpace::new();
        bus.add_ram("ram", BASE, ram.clone()).unwrap();
        let dma = DmaRam::new(BASE, ram, bus.reservation_invalidator());
        (bus, dma)
    }

    #[test]
    fn same_value_dma_write_invalidates_reservation() {
        let (bus, dma) = dma_space(0x1000);
        let addr = BASE + 0x100;
        bus.write(addr, AccessSize::Word, 0x1122_3344).unwrap();
        let seq = bus.reserve_cache_line(addr);

        dma.write_u32(addr, 0x1122_3344).unwrap();

        assert!(
            !bus.store_conditional(
                addr,
                AccessSize::Word,
                Some((seq, 0x1122_3344)),
                0x5566_7788,
            )
            .unwrap()
        );
        assert_eq!(bus.read(addr, AccessSize::Word).unwrap(), 0x1122_3344);
    }

    #[test]
    fn dma_range_invalidates_each_cache_line_it_touches() {
        let (bus, dma) = dma_space(0x1000);
        let first = BASE + 0x138;
        let second = BASE + 0x140;
        bus.write(first, AccessSize::Double, 0x1111).unwrap();
        bus.write(second, AccessSize::Double, 0x2222).unwrap();
        let first_seq = bus.reserve_cache_line(first);
        let second_seq = bus.reserve_cache_line(second);

        // Rewrite the existing bytes exactly so value comparison cannot mask
        // a missing reservation invalidation on either cache line.
        dma.write_bytes(BASE + 0x13c, &[0, 0, 0, 0, 0x22, 0x22, 0, 0])
            .unwrap();

        assert!(
            !bus.store_conditional(first, AccessSize::Double, Some((first_seq, 0x1111)), 0x3333,)
                .unwrap()
        );
        assert!(
            !bus.store_conditional(
                second,
                AccessSize::Double,
                Some((second_seq, 0x2222)),
                0x4444,
            )
            .unwrap()
        );
    }
}

/// One segment in a descriptor chain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DescSegment {
    /// Physical address in guest RAM.
    pub addr: GuestAddr,
    /// Length in bytes.
    pub len: u32,
}

/// A popped descriptor chain split into device-readable and device-writable segments.
#[derive(Clone, Debug)]
pub struct DescChain {
    /// Descriptor head index in the descriptor table.
    pub head: u16,
    /// Device-readable segments (`VIRTQ_DESC_F_WRITE == 0`).
    pub readable: Vec<DescSegment>,
    /// Device-writable segments (`VIRTQ_DESC_F_WRITE != 0`).
    pub writable: Vec<DescSegment>,
}

impl DescChain {
    /// Concatenates all readable segments into a contiguous byte vector.
    pub fn read_all(&self, dma: &DmaRam) -> Option<Vec<u8>> {
        let total: usize = self.readable.iter().map(|s| s.len as usize).sum();
        let mut out = vec![0u8; total];
        let mut pos = 0;
        for seg in &self.readable {
            let n = seg.len as usize;
            dma.read_bytes(seg.addr, &mut out[pos..pos + n])?;
            pos += n;
        }
        Some(out)
    }

    /// Total writable capacity in bytes across all writable segments.
    pub fn writable_len(&self) -> usize {
        self.writable.iter().map(|s| s.len as usize).sum()
    }

    /// Writes `data` across the writable segments, returning the number of bytes written.
    pub fn write_all(&self, dma: &DmaRam, data: &[u8]) -> Option<u32> {
        let mut written = 0usize;
        for seg in &self.writable {
            if written >= data.len() {
                break;
            }
            let n = (seg.len as usize).min(data.len() - written);
            dma.write_bytes(seg.addr, &data[written..written + n])?;
            written += n;
        }
        Some(written as u32)
    }
}

/// State of a single split virtqueue.
#[derive(Clone, Debug)]
pub struct VirtQueue {
    /// Queue size set by driver (`QueueNum`).
    pub num: u32,
    /// `QueueReady` flag.
    pub ready: bool,
    /// Descriptor table physical address.
    pub desc_addr: GuestAddr,
    /// Available ring physical address.
    pub avail_addr: GuestAddr,
    /// Used ring physical address.
    pub used_addr: GuestAddr,
    /// Next entry in `avail.ring` to consume.
    pub last_avail_idx: u16,
}

impl Default for VirtQueue {
    fn default() -> Self {
        Self {
            num: QUEUE_MAX_SIZE,
            ready: false,
            desc_addr: 0,
            avail_addr: 0,
            used_addr: 0,
            last_avail_idx: 0,
        }
    }
}

impl VirtQueue {
    /// Resets the queue to its initial unconfigured state.
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Whether there is at least one unconsumed descriptor chain in the avail ring.
    pub fn has_available(&self, dma: &DmaRam) -> bool {
        if !self.ready || self.num == 0 {
            return false;
        }
        let Some(avail_idx) = dma.read_u16(self.avail_addr + 2) else {
            return false;
        };
        avail_idx != self.last_avail_idx
    }

    /// Pops the next available descriptor chain from the queue.
    pub fn pop(&mut self, dma: &DmaRam) -> Option<DescChain> {
        if !self.ready || self.num == 0 {
            return None;
        }
        let avail_idx = dma.read_u16(self.avail_addr + 2)?;
        if avail_idx == self.last_avail_idx {
            return None;
        }
        let ring_slot = u64::from(self.last_avail_idx % self.num as u16);
        let head = dma.read_u16(self.avail_addr + 4 + ring_slot * 2)?;
        self.last_avail_idx = self.last_avail_idx.wrapping_add(1);

        let mut readable = Vec::new();
        let mut writable = Vec::new();
        self.walk_table(
            dma,
            self.desc_addr,
            self.num as u16,
            head,
            &mut readable,
            &mut writable,
        )?;
        Some(DescChain {
            head,
            readable,
            writable,
        })
    }

    fn walk_table(
        &self,
        dma: &DmaRam,
        table_base: GuestAddr,
        table_len: u16,
        start_idx: u16,
        readable: &mut Vec<DescSegment>,
        writable: &mut Vec<DescSegment>,
    ) -> Option<()> {
        let mut idx = start_idx;
        for _ in 0..table_len {
            if idx >= table_len {
                return None;
            }
            let entry = table_base + u64::from(idx) * 16;
            let addr = dma.read_u64(entry)?;
            let len = dma.read_u32(entry + 8)?;
            let flags = dma.read_u16(entry + 12)?;
            let next = dma.read_u16(entry + 14)?;

            if flags & VIRTQ_DESC_F_INDIRECT != 0 {
                let indir_count = (len / 16) as u16;
                if indir_count > 0 {
                    self.walk_table(dma, addr, indir_count, 0, readable, writable)?;
                }
            } else if flags & VIRTQ_DESC_F_WRITE != 0 {
                writable.push(DescSegment { addr, len });
            } else {
                readable.push(DescSegment { addr, len });
            }

            if flags & VIRTQ_DESC_F_NEXT == 0 {
                return Some(());
            }
            idx = next;
        }
        None
    }

    /// Pushes a completed descriptor chain to the `Used` ring.
    pub fn push_used(&mut self, dma: &DmaRam, head: u16, len: u32, event_idx: bool) {
        if self.num == 0 {
            return;
        }
        let Some(used_idx) = dma.read_u16(self.used_addr + 2) else {
            return;
        };
        let slot = u64::from(used_idx % self.num as u16);
        let elem_addr = self.used_addr + 4 + slot * 8;
        let _ = dma.write_u32(elem_addr, u32::from(head));
        let _ = dma.write_u32(elem_addr + 4, len);
        let new_used = used_idx.wrapping_add(1);
        let _ = dma.write_u16(self.used_addr + 2, new_used);
        if event_idx {
            // Write avail_event at the end of the used ring so the guest knows
            // our current last_avail_idx.
            let avail_event_addr = self.used_addr + 4 + u64::from(self.num) * 8;
            let _ = dma.write_u16(avail_event_addr, self.last_avail_idx);
        }
    }
}
