//! Guest RAM backed by an anonymous host mapping.
//!
//! Guest RAM may be accessed concurrently by several vCPU threads and by
//! device backends (DMA). To keep this sound under Rust's memory model, every
//! access goes through relaxed atomic operations: aligned accesses use a
//! single atomic of the matching width (which compiles to a plain load/store
//! on arm64 and x86_64), unaligned accesses fall back to byte-wise atomics.
//! No `&[u8]`/`&mut [u8]` view of guest memory is ever handed out.

use std::io;
use std::sync::atomic::{AtomicU8, AtomicU16, AtomicU32, AtomicU64, Ordering};

use memmap2::MmapMut;
use rivemu_core::AccessSize;

/// A contiguous block of guest RAM.
pub struct Ram {
    /// Keeps the mapping alive. Never accessed directly after construction:
    /// all accesses go through `ptr` with atomic operations.
    _map: MmapMut,
    ptr: *mut u8,
    len: u64,
}

// SAFETY: the mapping is owned by `Ram` and every access through `ptr` uses
// atomic operations, so concurrent access from multiple threads is sound.
unsafe impl Send for Ram {}
// SAFETY: see `Send` above.
unsafe impl Sync for Ram {}

impl std::fmt::Debug for Ram {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ram").field("len", &self.len).finish()
    }
}

impl Ram {
    /// Allocates `size` bytes of zero-initialised RAM.
    ///
    /// The host mapping is lazy, so large guest RAM sizes only consume host
    /// memory for pages the guest actually touches.
    pub fn new(size: u64) -> io::Result<Self> {
        if size == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "RAM size must be non-zero",
            ));
        }
        let len = usize::try_from(size).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "RAM size exceeds host address space",
            )
        })?;
        let mut map = MmapMut::map_anon(len)?;
        let ptr = map.as_mut_ptr();
        Ok(Self {
            _map: map,
            ptr,
            len: size,
        })
    }

    /// Size in bytes.
    #[inline]
    pub fn len(&self) -> u64 {
        self.len
    }

    /// Always `false`: zero-sized RAM cannot be created.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[inline]
    fn in_bounds(&self, offset: u64, n: u64) -> bool {
        offset.checked_add(n).is_some_and(|end| end <= self.len)
    }

    /// Pointer to byte `offset`. Caller must have bounds-checked.
    #[inline]
    fn at(&self, offset: u64) -> *mut u8 {
        debug_assert!(offset <= self.len);
        // SAFETY: offset <= len, and len fits in usize (checked in `new`),
        // so the result stays within (or one past) the mapping.
        unsafe { self.ptr.add(offset as usize) }
    }

    /// Little-endian read of `size` bytes at `offset`; `None` if out of bounds.
    #[inline]
    pub fn read(&self, offset: u64, size: AccessSize) -> Option<u64> {
        if !self.in_bounds(offset, size.bytes()) {
            return None;
        }
        // SAFETY: bounds checked above; pointer is valid for `size` bytes.
        Some(unsafe { load(self.at(offset), size) })
    }

    /// Little-endian write of the low `size` bytes of `value` at `offset`;
    /// `None` if out of bounds.
    #[inline]
    pub fn write(&self, offset: u64, size: AccessSize, value: u64) -> Option<()> {
        if !self.in_bounds(offset, size.bytes()) {
            return None;
        }
        // SAFETY: bounds checked above; pointer is valid for `size` bytes.
        unsafe { store(self.at(offset), size, value) };
        Some(())
    }

    /// Copies guest bytes into `buf`; `None` if out of bounds.
    pub fn read_bytes(&self, offset: u64, buf: &mut [u8]) -> Option<()> {
        if !self.in_bounds(offset, buf.len() as u64) {
            return None;
        }
        let base = self.at(offset);
        for (i, b) in buf.iter_mut().enumerate() {
            // SAFETY: bounds checked above; byte atomics have no alignment needs.
            *b = unsafe { AtomicU8::from_ptr(base.add(i)) }.load(Ordering::Relaxed);
        }
        Some(())
    }

    /// Copies `data` into guest memory; `None` if out of bounds.
    pub fn write_bytes(&self, offset: u64, data: &[u8]) -> Option<()> {
        if !self.in_bounds(offset, data.len() as u64) {
            return None;
        }
        let base = self.at(offset);
        for (i, &b) in data.iter().enumerate() {
            // SAFETY: bounds checked above; byte atomics have no alignment needs.
            unsafe { AtomicU8::from_ptr(base.add(i)) }.store(b, Ordering::Relaxed);
        }
        Some(())
    }

    /// Sets `len` bytes starting at `offset` to `byte`; `None` if out of bounds.
    pub fn fill(&self, offset: u64, len: u64, byte: u8) -> Option<()> {
        if !self.in_bounds(offset, len) {
            return None;
        }
        let base = self.at(offset);
        for i in 0..len as usize {
            // SAFETY: bounds checked above; byte atomics have no alignment needs.
            unsafe { AtomicU8::from_ptr(base.add(i)) }.store(byte, Ordering::Relaxed);
        }
        Some(())
    }
}

/// # Safety
/// `p` must be valid for reads of `size` bytes.
#[inline]
unsafe fn load(p: *mut u8, size: AccessSize) -> u64 {
    // SAFETY (all arms): caller guarantees validity; wider atomics are only
    // used when `p` is suitably aligned.
    unsafe {
        match size {
            AccessSize::Byte => u64::from(AtomicU8::from_ptr(p).load(Ordering::Relaxed)),
            AccessSize::Half if p.cast::<u16>().is_aligned() => u64::from(u16::from_le(
                AtomicU16::from_ptr(p.cast()).load(Ordering::Relaxed),
            )),
            AccessSize::Word if p.cast::<u32>().is_aligned() => u64::from(u32::from_le(
                AtomicU32::from_ptr(p.cast()).load(Ordering::Relaxed),
            )),
            AccessSize::Double if p.cast::<u64>().is_aligned() => {
                u64::from_le(AtomicU64::from_ptr(p.cast()).load(Ordering::Relaxed))
            }
            _ => {
                let mut v = 0u64;
                for i in 0..size.bytes() as usize {
                    let b = AtomicU8::from_ptr(p.add(i)).load(Ordering::Relaxed);
                    v |= u64::from(b) << (8 * i);
                }
                v
            }
        }
    }
}

/// # Safety
/// `p` must be valid for writes of `size` bytes.
#[inline]
unsafe fn store(p: *mut u8, size: AccessSize, value: u64) {
    // SAFETY (all arms): caller guarantees validity; wider atomics are only
    // used when `p` is suitably aligned.
    unsafe {
        match size {
            AccessSize::Byte => AtomicU8::from_ptr(p).store(value as u8, Ordering::Relaxed),
            AccessSize::Half if p.cast::<u16>().is_aligned() => {
                AtomicU16::from_ptr(p.cast()).store((value as u16).to_le(), Ordering::Relaxed)
            }
            AccessSize::Word if p.cast::<u32>().is_aligned() => {
                AtomicU32::from_ptr(p.cast()).store((value as u32).to_le(), Ordering::Relaxed)
            }
            AccessSize::Double if p.cast::<u64>().is_aligned() => {
                AtomicU64::from_ptr(p.cast()).store(value.to_le(), Ordering::Relaxed)
            }
            _ => {
                for i in 0..size.bytes() as usize {
                    AtomicU8::from_ptr(p.add(i)).store((value >> (8 * i)) as u8, Ordering::Relaxed);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_initialised() {
        let ram = Ram::new(4096).unwrap();
        assert_eq!(ram.read(0, AccessSize::Double), Some(0));
        assert_eq!(ram.read(4088, AccessSize::Double), Some(0));
    }

    #[test]
    fn rejects_zero_size() {
        assert!(Ram::new(0).is_err());
    }

    #[test]
    fn little_endian_layout() {
        let ram = Ram::new(64).unwrap();
        ram.write(0, AccessSize::Word, 0x1122_3344).unwrap();
        let mut buf = [0u8; 4];
        ram.read_bytes(0, &mut buf).unwrap();
        assert_eq!(buf, [0x44, 0x33, 0x22, 0x11]);
        assert_eq!(ram.read(1, AccessSize::Half), Some(0x2233));
    }

    #[test]
    fn unaligned_access() {
        let ram = Ram::new(64).unwrap();
        ram.write(3, AccessSize::Double, 0x0102_0304_0506_0708)
            .unwrap();
        assert_eq!(ram.read(3, AccessSize::Double), Some(0x0102_0304_0506_0708));
        assert_eq!(ram.read(3, AccessSize::Byte), Some(0x08));
        assert_eq!(ram.read(10, AccessSize::Byte), Some(0x01));
    }

    #[test]
    fn write_truncates_value() {
        let ram = Ram::new(16).unwrap();
        ram.write(0, AccessSize::Double, u64::MAX).unwrap();
        ram.write(0, AccessSize::Byte, 0x1234).unwrap();
        assert_eq!(ram.read(0, AccessSize::Word), Some(0xffff_ff34));
    }

    #[test]
    fn bounds_checks() {
        let ram = Ram::new(16).unwrap();
        assert_eq!(ram.read(9, AccessSize::Double), None);
        assert_eq!(ram.read(8, AccessSize::Double), Some(0));
        assert_eq!(ram.write(16, AccessSize::Byte, 0), None);
        assert_eq!(ram.read(u64::MAX, AccessSize::Word), None);
        assert_eq!(ram.write_bytes(10, &[0; 7]), None);
        assert_eq!(ram.fill(0, 17, 0), None);
    }

    #[test]
    fn fill_and_bytes() {
        let ram = Ram::new(32).unwrap();
        ram.fill(4, 8, 0xaa).unwrap();
        ram.write_bytes(6, &[1, 2]).unwrap();
        let mut buf = [0u8; 10];
        ram.read_bytes(3, &mut buf).unwrap();
        assert_eq!(buf, [0, 0xaa, 0xaa, 1, 2, 0xaa, 0xaa, 0xaa, 0xaa, 0]);
    }
}
