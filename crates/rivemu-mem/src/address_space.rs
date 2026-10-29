//! The guest physical address space: a sorted, non-overlapping set of regions.

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use rivemu_core::{AccessSize, GuestAddr};
use thiserror::Error;

use crate::{MmioDevice, Ram};

/// Number of 64-byte cache-line buckets in the reservation directory.
const RESERVATION_BUCKETS: usize = 4096;

/// Cloneable handle that lets external memory writers invalidate CPU LR/SC
/// reservations without owning the address space itself.
///
/// Handles can only be obtained from [`AddressSpace::reservation_invalidator`],
/// so every clone shares that address space's reservation directory.
#[derive(Clone)]
pub struct ReservationInvalidator {
    line_seq: Arc<[AtomicU64]>,
}

impl fmt::Debug for ReservationInvalidator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReservationInvalidator")
            .finish_non_exhaustive()
    }
}

impl ReservationInvalidator {
    /// Invalidates active reservations on every 64-byte cache line touched by
    /// `[addr, addr + len)`. A zero-length range has no effect; overflowing
    /// ranges are conservatively invalidated through the end of the address
    /// space.
    pub fn invalidate_range(&self, addr: GuestAddr, len: u64) {
        invalidate_reservation_range(&self.line_seq, addr, len);
    }
}

fn invalidate_reservation_range(line_seq: &[AtomicU64], addr: GuestAddr, len: u64) {
    if len == 0 {
        return;
    }
    let first_line = addr >> 6;
    let last_line = addr.saturating_add(len - 1) >> 6;
    // The directory is hash-bucketed, so ranges spanning a full directory only
    // need to visit each bucket once. Besides avoiding duplicate work, this
    // keeps a malformed overflowing DMA range from causing an enormous loop.
    let line_count = last_line
        .saturating_sub(first_line)
        .saturating_add(1)
        .min(RESERVATION_BUCKETS as u64);
    for line_offset in 0..line_count {
        let line = first_line + line_offset;
        let idx = (line as usize) & (RESERVATION_BUCKETS - 1);
        let seq = line_seq[idx].load(Ordering::Relaxed);
        if seq & 1 != 0 {
            invalidate_bucket_slow(line_seq, idx, seq);
        }
    }
}

#[cold]
fn invalidate_bucket_slow(line_seq: &[AtomicU64], idx: usize, seq: u64) {
    let _ = line_seq[idx].compare_exchange(
        seq,
        seq.wrapping_add(1) & !1,
        Ordering::SeqCst,
        Ordering::Relaxed,
    );
}

/// Error returned by guest-visible bus accesses. The CPU turns these into
/// access-fault exceptions.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum BusError {
    /// Nothing is mapped at the address.
    #[error("unmapped address {addr:#x} ({size} bytes)")]
    Unmapped {
        /// Faulting address.
        addr: GuestAddr,
        /// Access width in bytes.
        size: u64,
    },
    /// The access starts inside a region but extends past its end.
    #[error("access at {addr:#x} ({size} bytes) crosses a region boundary")]
    CrossesBoundary {
        /// Faulting address.
        addr: GuestAddr,
        /// Access width in bytes.
        size: u64,
    },
    /// Guest write to a ROM region.
    #[error("write to read-only memory at {addr:#x}")]
    ReadOnly {
        /// Faulting address.
        addr: GuestAddr,
    },
    /// A bulk (loader / DMA) access hit an MMIO region.
    #[error("address {addr:#x} is not backed by memory")]
    NotMemory {
        /// Faulting address.
        addr: GuestAddr,
    },
}

/// Error returned when adding a region to the address space.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum MapError {
    /// Region has size zero.
    #[error("region '{name}' has zero size")]
    ZeroSize {
        /// Region name.
        name: String,
    },
    /// Region extends past the end of the 64-bit address space.
    #[error("region '{name}' at {base:#x} overflows the address space")]
    Overflow {
        /// Region name.
        name: String,
        /// Region base.
        base: GuestAddr,
    },
    /// Region overlaps an existing one.
    #[error("region '{name}' at {base:#x} overlaps region '{other}'")]
    Overlap {
        /// Region name.
        name: String,
        /// Region base.
        base: GuestAddr,
        /// Name of the existing region.
        other: String,
    },
}

/// What backs a region.
#[derive(Clone)]
pub enum RegionKind {
    /// Read-write RAM.
    Ram(Arc<Ram>),
    /// Read-only memory (writable only through loader APIs).
    Rom(Arc<Ram>),
    /// Memory-mapped device.
    Mmio(Arc<dyn MmioDevice>),
}

impl fmt::Debug for RegionKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Ram(_) => "ram",
            Self::Rom(_) => "rom",
            Self::Mmio(_) => "i/o",
        })
    }
}

/// A mapped region of the address space.
#[derive(Debug, Clone)]
pub struct Region {
    /// Human-readable name (shown in `info mtree`-style dumps).
    pub name: String,
    /// First guest physical address.
    pub base: GuestAddr,
    /// Size in bytes (never zero).
    pub size: u64,
    /// Backing.
    pub kind: RegionKind,
}

impl Region {
    /// Last address covered by the region (inclusive).
    #[inline]
    pub fn last(&self) -> GuestAddr {
        self.base + (self.size - 1)
    }

    #[inline]
    fn contains(&self, addr: GuestAddr) -> bool {
        addr.wrapping_sub(self.base) < self.size
    }

    /// Whether `[addr, addr + n)` lies fully inside the region (`addr` must be contained).
    #[inline]
    fn fits(&self, addr: GuestAddr, n: u64) -> bool {
        n <= self.size - (addr - self.base)
    }
}

/// The guest physical address space.
#[derive(Debug)]
pub struct AddressSpace {
    /// Sorted by `base`, non-overlapping.
    regions: Vec<Region>,
    /// Largest mapped RAM region `(base, size, ram)` for O(1) main-memory access.
    fast_ram: Option<(GuestAddr, u64, Arc<Ram>)>,
    /// 64-byte cache-line reservation sequence counters `(gen << 1) | active_bit`.
    line_seq: Arc<[AtomicU64]>,
}

impl Default for AddressSpace {
    fn default() -> Self {
        Self {
            regions: Vec::new(),
            fast_ram: None,
            line_seq: Arc::from(
                (0..RESERVATION_BUCKETS)
                    .map(|_| AtomicU64::new(0))
                    .collect::<Vec<_>>()
                    .into_boxed_slice(),
            ),
        }
    }
}

impl AddressSpace {
    /// Creates an empty address space.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns a cloneable handle for invalidating this address space's LR/SC
    /// reservations after DMA writes or external machine-state replacement.
    pub fn reservation_invalidator(&self) -> ReservationInvalidator {
        ReservationInvalidator {
            line_seq: self.line_seq.clone(),
        }
    }

    /// Marks the 64-byte cache line containing `addr` as having an active `LR`
    /// reservation and returns its odd sequence token `(gen << 1) | 1`.
    #[inline]
    pub fn reserve_cache_line(&self, addr: GuestAddr) -> u64 {
        let idx = ((addr >> 6) as usize) & (RESERVATION_BUCKETS - 1);
        self.line_seq[idx].fetch_or(1, Ordering::SeqCst) | 1
    }

    /// Invalidates any active `LR` reservation on the 64-byte cache line(s)
    /// touched by a write of `size` bytes at `addr`.
    #[inline]
    pub fn invalidate_cache_line(&self, addr: GuestAddr, size: AccessSize) {
        invalidate_reservation_range(&self.line_seq, addr, size.bytes());
    }

    /// All regions, sorted by base address.
    pub fn regions(&self) -> &[Region] {
        &self.regions
    }

    /// Maps RAM at `base`.
    pub fn add_ram(&mut self, name: &str, base: GuestAddr, ram: Arc<Ram>) -> Result<(), MapError> {
        let size = ram.len();
        self.insert(name, base, size, RegionKind::Ram(ram.clone()))?;
        if self.fast_ram.as_ref().is_none_or(|(_, s, _)| size > *s) {
            self.fast_ram = Some((base, size, ram));
        }
        Ok(())
    }

    /// Maps ROM at `base`.
    pub fn add_rom(&mut self, name: &str, base: GuestAddr, rom: Arc<Ram>) -> Result<(), MapError> {
        let size = rom.len();
        self.insert(name, base, size, RegionKind::Rom(rom))
    }

    /// Maps a device at `[base, base + size)`.
    pub fn add_mmio(
        &mut self,
        name: &str,
        base: GuestAddr,
        size: u64,
        dev: Arc<dyn MmioDevice>,
    ) -> Result<(), MapError> {
        self.insert(name, base, size, RegionKind::Mmio(dev))
    }

    fn insert(
        &mut self,
        name: &str,
        base: GuestAddr,
        size: u64,
        kind: RegionKind,
    ) -> Result<(), MapError> {
        if size == 0 {
            return Err(MapError::ZeroSize { name: name.into() });
        }
        let last = base
            .checked_add(size - 1)
            .ok_or_else(|| MapError::Overflow {
                name: name.into(),
                base,
            })?;
        let pos = self.regions.partition_point(|r| r.base <= base);
        let overlap = pos
            .checked_sub(1)
            .map(|i| &self.regions[i])
            .filter(|prev| prev.last() >= base)
            .or_else(|| self.regions.get(pos).filter(|next| next.base <= last));
        if let Some(other) = overlap {
            return Err(MapError::Overlap {
                name: name.into(),
                base,
                other: other.name.clone(),
            });
        }
        self.regions.insert(
            pos,
            Region {
                name: name.into(),
                base,
                size,
                kind,
            },
        );
        Ok(())
    }

    /// Finds the region containing `addr`.
    #[inline]
    pub fn find(&self, addr: GuestAddr) -> Option<&Region> {
        let pos = self.regions.partition_point(|r| r.base <= addr);
        let r = &self.regions[pos.checked_sub(1)?];
        r.contains(addr).then_some(r)
    }

    #[inline]
    fn find_access(&self, addr: GuestAddr, size: AccessSize) -> Result<&Region, BusError> {
        let n = size.bytes();
        let r = self
            .find(addr)
            .ok_or(BusError::Unmapped { addr, size: n })?;
        if r.fits(addr, n) {
            Ok(r)
        } else {
            Err(BusError::CrossesBoundary { addr, size: n })
        }
    }

    /// Guest-visible read.
    #[inline]
    pub fn read(&self, addr: GuestAddr, size: AccessSize) -> Result<u64, BusError> {
        if let Some((base, len, ref ram)) = self.fast_ram {
            let off = addr.wrapping_sub(base);
            if off < len {
                return ram.read(off, size).ok_or(BusError::CrossesBoundary {
                    addr,
                    size: size.bytes(),
                });
            }
        }
        self.read_slow(addr, size)
    }

    #[cold]
    fn read_slow(&self, addr: GuestAddr, size: AccessSize) -> Result<u64, BusError> {
        let r = self.find_access(addr, size)?;
        let off = addr - r.base;
        Ok(match &r.kind {
            RegionKind::Ram(m) | RegionKind::Rom(m) => {
                m.read(off, size).expect("region bounds checked")
            }
            RegionKind::Mmio(dev) => dev.read(off, size) & size.mask(),
        })
    }

    /// Guest-visible write.
    #[inline]
    pub fn write(&self, addr: GuestAddr, size: AccessSize, value: u64) -> Result<(), BusError> {
        if let Some((base, len, ref ram)) = self.fast_ram {
            let off = addr.wrapping_sub(base);
            if off < len {
                ram.write(off, size, value)
                    .ok_or(BusError::CrossesBoundary {
                        addr,
                        size: size.bytes(),
                    })?;
                self.invalidate_cache_line(addr, size);
                return Ok(());
            }
        }
        self.write_slow(addr, size, value)
    }

    #[cold]
    fn write_slow(&self, addr: GuestAddr, size: AccessSize, value: u64) -> Result<(), BusError> {
        let r = self.find_access(addr, size)?;
        let off = addr - r.base;
        match &r.kind {
            RegionKind::Ram(m) => {
                m.write(off, size, value).expect("region bounds checked");
                self.invalidate_cache_line(addr, size);
            }
            RegionKind::Rom(_) => return Err(BusError::ReadOnly { addr }),
            RegionKind::Mmio(dev) => dev.write(off, size, value & size.mask()),
        }
        Ok(())
    }

    /// Atomic store-conditional (`sc.w` / `sc.d`) honouring 64-byte cache-line
    /// reservation exclusivity and ABA detection.
    ///
    /// The target is validated even when the reservation has already failed:
    /// architecturally, an `SC` may not retire before passing memory-permission
    /// checks.
    #[inline]
    pub fn store_conditional(
        &self,
        addr: GuestAddr,
        size: AccessSize,
        reservation: Option<(u64, u64)>,
        new: u64,
    ) -> Result<bool, BusError> {
        let target = self.find_access(addr, size)?;
        match &target.kind {
            RegionKind::Ram(_) => {}
            RegionKind::Rom(_) => return Err(BusError::ReadOnly { addr }),
            RegionKind::Mmio(_) => return Err(BusError::NotMemory { addr }),
        }

        let Some((expected_seq, expected_val)) = reservation else {
            return Ok(false);
        };
        let idx = ((addr >> 6) as usize) & (RESERVATION_BUCKETS - 1);
        if self.line_seq[idx]
            .compare_exchange(
                expected_seq,
                expected_seq.wrapping_add(1) & !1,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_err()
        {
            return Ok(false);
        }
        self.compare_exchange(addr, size, expected_val, new)
    }

    /// Atomic compare-and-exchange on guest RAM (`sc.w` / `sc.d`).
    #[inline]
    pub fn compare_exchange(
        &self,
        addr: GuestAddr,
        size: AccessSize,
        expected: u64,
        new: u64,
    ) -> Result<bool, BusError> {
        let ok = if let Some((base, len, ref ram)) = self.fast_ram {
            let off = addr.wrapping_sub(base);
            if off < len {
                let ok = ram.compare_exchange(off, size, expected, new).ok_or(
                    BusError::CrossesBoundary {
                        addr,
                        size: size.bytes(),
                    },
                )?;
                if ok {
                    self.invalidate_cache_line(addr, size);
                }
                return Ok(ok);
            }
            false
        } else {
            false
        };
        let _ = ok;
        let r = self.find_access(addr, size)?;
        let off = addr - r.base;
        match &r.kind {
            RegionKind::Ram(m) => {
                let ok = m.compare_exchange(off, size, expected, new).ok_or(
                    BusError::CrossesBoundary {
                        addr,
                        size: size.bytes(),
                    },
                )?;
                if ok {
                    self.invalidate_cache_line(addr, size);
                }
                Ok(ok)
            }
            RegionKind::Rom(_) => Err(BusError::ReadOnly { addr }),
            RegionKind::Mmio(_) => Err(BusError::NotMemory { addr }),
        }
    }

    /// Atomic read-modify-write on guest RAM (`amo*.w` / `amo*.d`).
    #[inline]
    pub fn atomic_rmw(
        &self,
        addr: GuestAddr,
        size: AccessSize,
        f: impl FnMut(u64) -> u64,
    ) -> Result<u64, BusError> {
        if let Some((base, len, ref ram)) = self.fast_ram {
            let off = addr.wrapping_sub(base);
            if off < len {
                let old = ram
                    .atomic_rmw(off, size, f)
                    .ok_or(BusError::CrossesBoundary {
                        addr,
                        size: size.bytes(),
                    })?;
                self.invalidate_cache_line(addr, size);
                return Ok(old);
            }
        }
        let r = self.find_access(addr, size)?;
        let off = addr - r.base;
        match &r.kind {
            RegionKind::Ram(m) => {
                let old = m
                    .atomic_rmw(off, size, f)
                    .ok_or(BusError::CrossesBoundary {
                        addr,
                        size: size.bytes(),
                    })?;
                self.invalidate_cache_line(addr, size);
                Ok(old)
            }
            RegionKind::Rom(_) => Err(BusError::ReadOnly { addr }),
            RegionKind::Mmio(_) => Err(BusError::NotMemory { addr }),
        }
    }

    /// Applies `f(memory, offset_in_region, offset_in_request, chunk_len)` to
    /// every memory chunk covering `[addr, addr + len)`. Spans adjacent
    /// RAM/ROM regions; fails on holes or MMIO.
    fn for_each_chunk(
        &self,
        addr: GuestAddr,
        len: u64,
        mut f: impl FnMut(&Ram, u64, usize, u64),
    ) -> Result<(), BusError> {
        let mut done = 0u64;
        while done < len {
            let cur = addr.checked_add(done).ok_or(BusError::Unmapped {
                addr: u64::MAX,
                size: 1,
            })?;
            let r = self
                .find(cur)
                .ok_or(BusError::Unmapped { addr: cur, size: 1 })?;
            let mem = match &r.kind {
                RegionKind::Ram(m) | RegionKind::Rom(m) => m,
                RegionKind::Mmio(_) => return Err(BusError::NotMemory { addr: cur }),
            };
            let off = cur - r.base;
            let chunk = (len - done).min(r.size - off);
            f(mem, off, done as usize, chunk);
            done += chunk;
        }
        Ok(())
    }

    /// Writes bytes into RAM/ROM (loaders and debugger). Ignores ROM write protection.
    pub fn load_bytes(&self, addr: GuestAddr, data: &[u8]) -> Result<(), BusError> {
        self.for_each_chunk(addr, data.len() as u64, |m, off, start, n| {
            m.write_bytes(off, &data[start..start + n as usize])
                .expect("region bounds checked");
            invalidate_reservation_range(&self.line_seq, addr + start as u64, n);
        })
    }

    /// Reads bytes from RAM/ROM (loaders, DMA, debugger).
    pub fn read_bytes(&self, addr: GuestAddr, buf: &mut [u8]) -> Result<(), BusError> {
        self.for_each_chunk(addr, buf.len() as u64, |m, off, start, n| {
            m.read_bytes(off, &mut buf[start..start + n as usize])
                .expect("region bounds checked");
        })
    }

    /// Fills `[addr, addr + len)` of RAM/ROM with `byte`.
    pub fn fill_bytes(&self, addr: GuestAddr, len: u64, byte: u8) -> Result<(), BusError> {
        self.for_each_chunk(addr, len, |m, off, start, n| {
            m.fill(off, n, byte).expect("region bounds checked");
            invalidate_reservation_range(&self.line_seq, addr + start as u64, n);
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Recorder {
        log: Mutex<Vec<(u64, AccessSize, Option<u64>)>>,
    }

    impl MmioDevice for Recorder {
        fn read(&self, offset: u64, size: AccessSize) -> u64 {
            self.log.lock().unwrap().push((offset, size, None));
            0xdead_beef_cafe_f00d
        }
        fn write(&self, offset: u64, size: AccessSize, value: u64) {
            self.log.lock().unwrap().push((offset, size, Some(value)));
        }
    }

    fn ram(size: u64) -> Arc<Ram> {
        Arc::new(Ram::new(size).unwrap())
    }

    fn space() -> (AddressSpace, Arc<Recorder>) {
        let mut a = AddressSpace::new();
        let dev = Arc::new(Recorder::default());
        a.add_ram("dram", 0x8000_0000, ram(0x1000)).unwrap();
        a.add_rom("mrom", 0x1000, ram(0x100)).unwrap();
        a.add_mmio("uart", 0x1000_0000, 0x100, dev.clone()).unwrap();
        (a, dev)
    }

    #[test]
    fn regions_sorted() {
        let (a, _) = space();
        let names: Vec<_> = a.regions().iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["mrom", "uart", "dram"]);
    }

    #[test]
    fn overlap_rejected() {
        let (mut a, _) = space();
        for (base, size) in [
            (0x8000_0fff, 1),
            (0x7fff_ffff, 2),
            (0x7000_0000, 0x2000_0000),
            (0x8000_0800, 0x10),
        ] {
            let err = a.add_ram("x", base, ram(size)).unwrap_err();
            assert!(matches!(err, MapError::Overlap { .. }), "{base:#x}: {err}");
        }
        a.add_ram("adjacent", 0x8000_1000, ram(0x10)).unwrap();
        a.add_ram("before", 0x7fff_f000, ram(0x1000)).unwrap();
    }

    #[test]
    fn zero_size_and_overflow_rejected() {
        let mut a = AddressSpace::new();
        let dev: Arc<dyn MmioDevice> = Arc::new(Recorder::default());
        assert!(matches!(
            a.add_mmio("z", 0, 0, dev.clone()),
            Err(MapError::ZeroSize { .. })
        ));
        assert!(matches!(
            a.add_mmio("o", u64::MAX, 2, dev.clone()),
            Err(MapError::Overflow { .. })
        ));
        a.add_mmio("top", u64::MAX, 1, dev).unwrap();
    }

    #[test]
    fn ram_read_write() {
        let (a, _) = space();
        a.write(0x8000_0010, AccessSize::Double, 0x1122_3344_5566_7788)
            .unwrap();
        assert_eq!(a.read(0x8000_0010, AccessSize::Word).unwrap(), 0x5566_7788);
        assert_eq!(a.read(0x8000_0ff8, AccessSize::Double).unwrap(), 0);
    }

    #[test]
    fn faults() {
        let (a, _) = space();
        assert_eq!(
            a.read(0x4000_0000, AccessSize::Word),
            Err(BusError::Unmapped {
                addr: 0x4000_0000,
                size: 4
            })
        );
        assert_eq!(
            a.read(0x8000_0ffc, AccessSize::Double),
            Err(BusError::CrossesBoundary {
                addr: 0x8000_0ffc,
                size: 8
            })
        );
        assert_eq!(
            a.write(0x1000, AccessSize::Byte, 1),
            Err(BusError::ReadOnly { addr: 0x1000 })
        );
        assert_eq!(
            a.read(0, AccessSize::Byte).unwrap_err().to_string(),
            "unmapped address 0x0 (1 bytes)"
        );
    }

    #[test]
    fn mmio_dispatch_masks_values() {
        let (a, dev) = space();
        assert_eq!(a.read(0x1000_0005, AccessSize::Byte).unwrap(), 0x0d);
        a.write(0x1000_0004, AccessSize::Half, 0x1234_5678).unwrap();
        let log = dev.log.lock().unwrap();
        assert_eq!(
            *log,
            [
                (5, AccessSize::Byte, None),
                (4, AccessSize::Half, Some(0x5678))
            ]
        );
    }

    #[test]
    fn loader_writes_rom_and_spans_regions() {
        let (mut a, _) = space();
        a.add_ram("dram2", 0x8000_1000, ram(0x1000)).unwrap();
        a.load_bytes(0x1000, &[1, 2, 3]).unwrap();
        assert_eq!(a.read(0x1001, AccessSize::Half).unwrap(), 0x0302);

        let data: Vec<u8> = (0..32).collect();
        a.load_bytes(0x8000_0ff0, &data).unwrap();
        let mut back = [0u8; 32];
        a.read_bytes(0x8000_0ff0, &mut back).unwrap();
        assert_eq!(&back[..], &data[..]);

        a.fill_bytes(0x8000_0ff8, 16, 0xee).unwrap();
        assert_eq!(a.read(0x8000_1000, AccessSize::Byte).unwrap(), 0xee);
    }

    #[test]
    fn loader_rejects_holes_and_mmio() {
        let (a, _) = space();
        assert_eq!(
            a.load_bytes(0x8000_0ffe, &[0; 4]),
            Err(BusError::Unmapped {
                addr: 0x8000_1000,
                size: 1
            })
        );
        assert_eq!(
            a.load_bytes(0x1000_0000, &[0]),
            Err(BusError::NotMemory { addr: 0x1000_0000 })
        );
        a.load_bytes(0x8000_0000, &[]).unwrap();
    }

    #[test]
    fn bulk_writes_invalidate_same_value_reservations() {
        let (a, _) = space();
        let loaded = 0x8000_0100;
        let filled = 0x8000_0180;
        a.write(loaded, AccessSize::Word, 0x1122_3344).unwrap();
        a.write(filled, AccessSize::Word, 0x5555_5555).unwrap();
        let loaded_seq = a.reserve_cache_line(loaded);
        let filled_seq = a.reserve_cache_line(filled);

        a.load_bytes(loaded, &0x1122_3344u32.to_le_bytes()).unwrap();
        a.fill_bytes(filled, 4, 0x55).unwrap();

        assert!(
            !a.store_conditional(
                loaded,
                AccessSize::Word,
                Some((loaded_seq, 0x1122_3344)),
                0xdead_beef,
            )
            .unwrap()
        );
        assert!(
            !a.store_conditional(
                filled,
                AccessSize::Word,
                Some((filled_seq, 0x5555_5555)),
                0xdead_beef,
            )
            .unwrap()
        );
    }

    #[test]
    fn huge_external_range_visits_each_reservation_bucket_once() {
        let (a, _) = space();
        let addr = 0x8000_0100;
        a.write(addr, AccessSize::Word, 0x1122_3344).unwrap();
        let seq = a.reserve_cache_line(addr);

        a.reservation_invalidator().invalidate_range(0, u64::MAX);

        assert!(
            !a.store_conditional(
                addr,
                AccessSize::Word,
                Some((seq, 0x1122_3344)),
                0xdead_beef,
            )
            .unwrap()
        );
    }

    #[test]
    fn cache_line_reservation_and_aba_invalidation() {
        let (a, _) = space();
        let line0 = 0x8000_0100;
        a.write(line0, AccessSize::Double, 0x1111).unwrap();

        // 1. Store to a different word in the same 64-byte cache line invalidates LR.
        let seq = a.reserve_cache_line(line0);
        a.write(line0 + 32, AccessSize::Word, 0x9999).unwrap();
        assert!(
            !a.store_conditional(line0, AccessSize::Double, Some((seq, 0x1111)), 0x2222)
                .unwrap()
        );

        // 2. ABA store (writing the exact same value back to line0) invalidates LR.
        let seq = a.reserve_cache_line(line0);
        a.write(line0, AccessSize::Double, 0x1111).unwrap();
        assert!(
            !a.store_conditional(line0, AccessSize::Double, Some((seq, 0x1111)), 0x2222)
                .unwrap()
        );

        // 3. Store to a different 64-byte cache line (line0 + 64) preserves LR.
        let seq = a.reserve_cache_line(line0);
        a.write(line0 + 64, AccessSize::Double, 0x5555).unwrap();
        assert!(
            a.store_conditional(line0, AccessSize::Double, Some((seq, 0x1111)), 0x2222)
                .unwrap()
        );
        assert_eq!(a.read(line0, AccessSize::Double).unwrap(), 0x2222);
    }

    #[test]
    fn failed_store_conditional_still_checks_target_access() {
        let (a, _) = space();
        // There is no active reservation for any of these addresses.  SC must
        // nevertheless validate the target before retiring with a failure code.
        assert_eq!(
            a.store_conditional(0x4000_0000, AccessSize::Word, None, 0x1234),
            Err(BusError::Unmapped {
                addr: 0x4000_0000,
                size: 4
            })
        );
        assert_eq!(
            a.store_conditional(0x1000, AccessSize::Word, None, 0x1234),
            Err(BusError::ReadOnly { addr: 0x1000 })
        );
        assert_eq!(
            a.store_conditional(0x1000_0000, AccessSize::Word, None, 0x1234),
            Err(BusError::NotMemory { addr: 0x1000_0000 })
        );

        // A stale reservation on ordinary RAM remains a normal SC failure and
        // must leave memory untouched.
        a.write(0x8000_0200, AccessSize::Word, 0x5566_7788).unwrap();
        assert_eq!(
            a.store_conditional(0x8000_0200, AccessSize::Word, None, 0xdead_beef),
            Ok(false)
        );
        assert_eq!(a.read(0x8000_0200, AccessSize::Word).unwrap(), 0x5566_7788);
    }
}
