//! The guest physical address space: a sorted, non-overlapping set of regions.

use std::fmt;
use std::sync::Arc;

use rivemu_core::{AccessSize, GuestAddr};
use thiserror::Error;

use crate::{MmioDevice, Ram};

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
#[derive(Debug, Default)]
pub struct AddressSpace {
    /// Sorted by `base`, non-overlapping.
    regions: Vec<Region>,
    /// Largest mapped RAM region `(base, size, ram)` for O(1) main-memory access.
    fast_ram: Option<(GuestAddr, u64, Arc<Ram>)>,
}

impl AddressSpace {
    /// Creates an empty address space.
    pub fn new() -> Self {
        Self::default()
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
                return ram
                    .write(off, size, value)
                    .ok_or(BusError::CrossesBoundary {
                        addr,
                        size: size.bytes(),
                    });
            }
        }
        self.write_slow(addr, size, value)
    }

    #[cold]
    fn write_slow(&self, addr: GuestAddr, size: AccessSize, value: u64) -> Result<(), BusError> {
        let r = self.find_access(addr, size)?;
        let off = addr - r.base;
        match &r.kind {
            RegionKind::Ram(m) => m.write(off, size, value).expect("region bounds checked"),
            RegionKind::Rom(_) => return Err(BusError::ReadOnly { addr }),
            RegionKind::Mmio(dev) => dev.write(off, size, value & size.mask()),
        }
        Ok(())
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
        if let Some((base, len, ref ram)) = self.fast_ram {
            let off = addr.wrapping_sub(base);
            if off < len {
                return ram.compare_exchange(off, size, expected, new).ok_or(
                    BusError::CrossesBoundary {
                        addr,
                        size: size.bytes(),
                    },
                );
            }
        }
        let r = self.find_access(addr, size)?;
        let off = addr - r.base;
        match &r.kind {
            RegionKind::Ram(m) => {
                m.compare_exchange(off, size, expected, new)
                    .ok_or(BusError::CrossesBoundary {
                        addr,
                        size: size.bytes(),
                    })
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
                return ram
                    .atomic_rmw(off, size, f)
                    .ok_or(BusError::CrossesBoundary {
                        addr,
                        size: size.bytes(),
                    });
            }
        }
        let r = self.find_access(addr, size)?;
        let off = addr - r.base;
        match &r.kind {
            RegionKind::Ram(m) => m.atomic_rmw(off, size, f).ok_or(BusError::CrossesBoundary {
                addr,
                size: size.bytes(),
            }),
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

    /// Writes bytes into RAM/ROM (loaders, DMA). Ignores ROM write protection.
    pub fn load_bytes(&self, addr: GuestAddr, data: &[u8]) -> Result<(), BusError> {
        self.for_each_chunk(addr, data.len() as u64, |m, off, start, n| {
            m.write_bytes(off, &data[start..start + n as usize])
                .expect("region bounds checked");
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
        self.for_each_chunk(addr, len, |m, off, _, n| {
            m.fill(off, n, byte).expect("region bounds checked");
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
}
