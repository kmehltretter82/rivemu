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
    /// Bitmap of 4 KiB pages that have been written since creation or reset.
    dirty: Box<[AtomicU64]>,
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
        let pages = size.div_ceil(4096);
        let words = pages.div_ceil(64) as usize;
        let dirty = (0..words)
            .map(|_| AtomicU64::new(0))
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Ok(Self {
            _map: map,
            ptr,
            len: size,
            dirty,
        })
    }

    #[inline(always)]
    fn mark_dirty_range(&self, offset: u64, len: u64) {
        // Fast path: an access within one page whose bit is already set.
        // Bits are only cleared while the machine is stopped (reset,
        // snapshot restore), so a set bit stays set while guests and devices
        // write, and testing first skips the locked read-modify-write.
        if len != 0 && (offset & 0xfff) + len <= 0x1000 {
            let p = (offset >> 12) as usize;
            if let Some(w) = self.dirty.get(p >> 6) {
                let bit = 1u64 << (p & 63);
                if w.load(Ordering::Relaxed) & bit == 0 {
                    w.fetch_or(bit, Ordering::Relaxed);
                }
            }
            return;
        }
        self.mark_dirty_pages(offset, len);
    }

    #[cold]
    fn mark_dirty_pages(&self, offset: u64, len: u64) {
        if len == 0 {
            return;
        }
        let first_page = (offset >> 12) as usize;
        let last_page = (offset.saturating_add(len - 1) >> 12) as usize;
        for p in first_page..=last_page {
            if let Some(w) = self.dirty.get(p >> 6) {
                let bit = 1u64 << (p & 63);
                if w.load(Ordering::Relaxed) & bit == 0 {
                    w.fetch_or(bit, Ordering::Relaxed);
                }
            }
        }
    }

    /// Returns the indices of all 4 KiB pages that have been written and are
    /// currently non-zero.
    pub fn dirty_non_zero_pages(&self) -> Vec<u32> {
        let total_pages = (self.len / 4096) as usize;
        let mut out = Vec::new();
        for (word_idx, atom) in self.dirty.iter().enumerate() {
            let mut bits = atom.load(Ordering::Relaxed);
            while bits != 0 {
                let bit = bits.trailing_zeros() as usize;
                bits &= bits - 1;
                let page = word_idx * 64 + bit;
                if page < total_pages && !self.is_page_zero((page as u64) * 4096) {
                    out.push(page as u32);
                }
            }
        }
        out
    }

    /// Zeroes all pages currently marked dirty and clears the dirty bitmap.
    pub fn clear_dirty_pages(&self) {
        let total_pages = (self.len / 4096) as usize;
        for (word_idx, atom) in self.dirty.iter().enumerate() {
            let mut bits = atom.swap(0, Ordering::Relaxed);
            while bits != 0 {
                let bit = bits.trailing_zeros() as usize;
                bits &= bits - 1;
                let page = word_idx * 64 + bit;
                if page < total_pages {
                    let base = self.at((page as u64) * 4096).cast::<u64>();
                    for i in 0..512 {
                        // SAFETY: page is within the page-aligned mmap.
                        unsafe { AtomicU64::from_ptr(base.add(i)) }.store(0, Ordering::Relaxed);
                    }
                }
            }
        }
    }

    /// Reads a 4 KiB page (`page_idx * 4096`) into `buf`.
    #[inline]
    pub fn read_page(&self, page_idx: u32, buf: &mut [u8; 4096]) {
        let offset = u64::from(page_idx) * 4096;
        if !self.in_bounds(offset, 4096) {
            return;
        }
        // SAFETY: `offset + 4096 <= self.len`, single-threaded snapshot read.
        unsafe {
            std::ptr::copy_nonoverlapping(self.at(offset), buf.as_mut_ptr(), 4096);
        }
    }

    /// Writes a 4 KiB page (`page_idx * 4096`) from `buf` and marks it dirty.
    #[inline]
    pub fn write_page(&self, page_idx: u32, buf: &[u8; 4096]) {
        let offset = u64::from(page_idx) * 4096;
        if !self.in_bounds(offset, 4096) {
            return;
        }
        let p = page_idx as usize;
        if let Some(w) = self.dirty.get(p >> 6) {
            w.fetch_or(1u64 << (p & 63), Ordering::Relaxed);
        }
        // SAFETY: `offset + 4096 <= self.len`, single-threaded snapshot restore.
        unsafe {
            std::ptr::copy_nonoverlapping(buf.as_ptr(), self.at(offset), 4096);
        }
    }

    /// Zeroes any dirty pages not present in the sorted `keep_pages` slice and
    /// resets the dirty bitmap to `keep_pages`.
    pub fn reset_dirty_for_restore(&self, keep_pages: &[u32]) {
        let total_pages = (self.len / 4096) as usize;
        for (word_idx, atom) in self.dirty.iter().enumerate() {
            let mut bits = atom.swap(0, Ordering::Relaxed);
            while bits != 0 {
                let bit = bits.trailing_zeros() as usize;
                bits &= bits - 1;
                let page = word_idx * 64 + bit;
                if page < total_pages && keep_pages.binary_search(&(page as u32)).is_err() {
                    // SAFETY: `page * 4096 + 4096 <= self.len`.
                    unsafe {
                        std::ptr::write_bytes(self.at((page as u64) * 4096), 0, 4096);
                    }
                }
            }
        }
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
    #[inline(always)]
    pub fn read(&self, offset: u64, size: AccessSize) -> Option<u64> {
        if !self.in_bounds(offset, size.bytes()) {
            return None;
        }
        // SAFETY: bounds checked above; pointer is valid for `size` bytes.
        Some(unsafe { load(self.at(offset), size) })
    }

    /// Little-endian write of the low `size` bytes of `value` at `offset`;
    /// `None` if out of bounds.
    #[inline(always)]
    pub fn write(&self, offset: u64, size: AccessSize, value: u64) -> Option<()> {
        if !self.in_bounds(offset, size.bytes()) {
            return None;
        }
        self.mark_dirty_range(offset, size.bytes());
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
        if offset & 7 == 0 && buf.len() & 7 == 0 {
            let base64 = base.cast::<u64>();
            for (i, chunk) in buf.chunks_exact_mut(8).enumerate() {
                // SAFETY: `self.ptr` is page-aligned and `offset` is 8-byte aligned.
                let w = u64::from_le(
                    unsafe { AtomicU64::from_ptr(base64.add(i)) }.load(Ordering::Relaxed),
                );
                chunk.copy_from_slice(&w.to_le_bytes());
            }
            return Some(());
        }
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
        self.mark_dirty_range(offset, data.len() as u64);
        let base = self.at(offset);
        if offset & 7 == 0 && data.len() & 7 == 0 {
            let base64 = base.cast::<u64>();
            for (i, chunk) in data.chunks_exact(8).enumerate() {
                let w = u64::from_le_bytes(chunk.try_into().expect("8-byte chunk"));
                // SAFETY: `self.ptr` is page-aligned and `offset` is 8-byte aligned.
                unsafe { AtomicU64::from_ptr(base64.add(i)) }.store(w.to_le(), Ordering::Relaxed);
            }
            return Some(());
        }
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
        if byte != 0 {
            self.mark_dirty_range(offset, len);
        }
        let base = self.at(offset);
        for i in 0..len as usize {
            // SAFETY: bounds checked above; byte atomics have no alignment needs.
            unsafe { AtomicU8::from_ptr(base.add(i)) }.store(byte, Ordering::Relaxed);
        }
        Some(())
    }

    /// Returns `true` if the 4 KiB page at `offset` (`offset % 4096 == 0`) is
    /// all zeroes.
    pub fn is_page_zero(&self, offset: u64) -> bool {
        if offset & 0xfff != 0 || !self.in_bounds(offset, 4096) {
            return false;
        }
        let base = self.at(offset).cast::<u64>();
        for i in 0..512 {
            // SAFETY: `self.ptr` is page-aligned (`mmap`) and `offset` is a
            // multiple of 4096 within bounds.
            let w = unsafe { AtomicU64::from_ptr(base.add(i)) }.load(Ordering::Relaxed);
            if w != 0 {
                return false;
            }
        }
        true
    }

    /// Atomic compare-and-exchange for aligned guest accesses
    /// (`sc.w` / `sc.d` / `amocas.*`). Returns `Some(true)` on success,
    /// `Some(false)` on mismatch, or `None` if out of bounds or unaligned.
    #[inline]
    pub fn compare_exchange(
        &self,
        offset: u64,
        size: AccessSize,
        expected: u64,
        new: u64,
    ) -> Option<bool> {
        if !self.in_bounds(offset, size.bytes()) {
            return None;
        }
        self.mark_dirty_range(offset, size.bytes());
        let p = self.at(offset);
        // SAFETY: bounds checked above; alignment verified in each arm.
        unsafe {
            match size {
                AccessSize::Byte => {
                    let atom = AtomicU8::from_ptr(p);
                    Some(
                        atom.compare_exchange(
                            expected as u8,
                            new as u8,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_ok(),
                    )
                }
                AccessSize::Half if p.cast::<u16>().is_aligned() => {
                    let atom = AtomicU16::from_ptr(p.cast());
                    Some(
                        atom.compare_exchange(
                            (expected as u16).to_le(),
                            (new as u16).to_le(),
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_ok(),
                    )
                }
                AccessSize::Word if p.cast::<u32>().is_aligned() => {
                    let atom = AtomicU32::from_ptr(p.cast());
                    Some(
                        atom.compare_exchange(
                            (expected as u32).to_le(),
                            (new as u32).to_le(),
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_ok(),
                    )
                }
                AccessSize::Double if p.cast::<u64>().is_aligned() => {
                    let atom = AtomicU64::from_ptr(p.cast());
                    Some(
                        atom.compare_exchange(
                            expected.to_le(),
                            new.to_le(),
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_ok(),
                    )
                }
                _ => None,
            }
        }
    }

    /// Atomic read-modify-write for aligned guest accesses
    /// (`amo*.b` / `amo*.h` / `amo*.w` / `amo*.d`). Returns the old value
    /// (`u64`), or `None` if out of bounds or unaligned.
    #[inline]
    pub fn atomic_rmw(
        &self,
        offset: u64,
        size: AccessSize,
        mut f: impl FnMut(u64) -> u64,
    ) -> Option<u64> {
        if !self.in_bounds(offset, size.bytes()) {
            return None;
        }
        self.mark_dirty_range(offset, size.bytes());
        let p = self.at(offset);
        // SAFETY: bounds checked above; alignment verified in each arm.
        unsafe {
            match size {
                AccessSize::Byte => {
                    let atom = AtomicU8::from_ptr(p);
                    let mut cur = atom.load(Ordering::Acquire);
                    loop {
                        let next = f(u64::from(cur)) as u8;
                        match atom.compare_exchange_weak(
                            cur,
                            next,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        ) {
                            Ok(_) => return Some(u64::from(cur)),
                            Err(actual) => cur = actual,
                        }
                    }
                }
                AccessSize::Half if p.cast::<u16>().is_aligned() => {
                    let atom = AtomicU16::from_ptr(p.cast());
                    let mut cur_le = atom.load(Ordering::Acquire);
                    loop {
                        let cur = u64::from(u16::from_le(cur_le));
                        let next_le = (f(cur) as u16).to_le();
                        match atom.compare_exchange_weak(
                            cur_le,
                            next_le,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        ) {
                            Ok(_) => return Some(cur),
                            Err(actual) => cur_le = actual,
                        }
                    }
                }
                AccessSize::Word if p.cast::<u32>().is_aligned() => {
                    let atom = AtomicU32::from_ptr(p.cast());
                    let mut cur_le = atom.load(Ordering::Acquire);
                    loop {
                        let cur = u64::from(u32::from_le(cur_le));
                        let next_le = (f(cur) as u32).to_le();
                        match atom.compare_exchange_weak(
                            cur_le,
                            next_le,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        ) {
                            Ok(_) => return Some(cur),
                            Err(actual) => cur_le = actual,
                        }
                    }
                }
                AccessSize::Double if p.cast::<u64>().is_aligned() => {
                    let atom = AtomicU64::from_ptr(p.cast());
                    let mut cur_le = atom.load(Ordering::Acquire);
                    loop {
                        let cur = u64::from_le(cur_le);
                        let next_le = f(cur).to_le();
                        match atom.compare_exchange_weak(
                            cur_le,
                            next_le,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        ) {
                            Ok(_) => return Some(cur),
                            Err(actual) => cur_le = actual,
                        }
                    }
                }
                _ => None,
            }
        }
    }

    /// Atomic read-modify-write of a misaligned `size`-byte field at
    /// `offset` that lies inside one naturally aligned 16-byte granule
    /// (Zama16b). Returns the old field value, or `None` if the field crosses
    /// the granule or is out of bounds.
    ///
    /// A field inside one aligned doubleword uses a 64-bit compare-and-swap.
    /// A field straddling the granule's two doublewords is updated with two
    /// stores, like [`Self::atomic_cas128`]: atomic because every hart and
    /// device of a machine runs on one thread (`Machine::run_slice`).
    pub fn atomic_rmw_granule(
        &self,
        offset: u64,
        size: AccessSize,
        mut f: impl FnMut(u64) -> u64,
    ) -> Option<u64> {
        let n = size.bytes();
        if (offset & 15) + n > 16 || !self.in_bounds(offset, n) {
            return None;
        }
        self.mark_dirty_range(offset, n);
        let mask = if n == 8 {
            u64::MAX
        } else {
            (1u64 << (8 * n)) - 1
        };
        let dword = offset & !7;
        if (offset - dword) + n <= 8 {
            let shift = 8 * (offset - dword);
            // SAFETY: `dword` is 8-byte aligned (the mapping is page-aligned)
            // and `dword + 8 <= offset + n <= len` or the field would cross
            // the granule end, which lies inside the mapping.
            let atom = unsafe { AtomicU64::from_ptr(self.at(dword).cast()) };
            let mut cur_le = atom.load(Ordering::Acquire);
            loop {
                let cur = u64::from_le(cur_le);
                let old = (cur >> shift) & mask;
                let next = (cur & !(mask << shift)) | ((f(old) & mask) << shift);
                match atom.compare_exchange_weak(
                    cur_le,
                    next.to_le(),
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => return Some(old),
                    Err(actual) => cur_le = actual,
                }
            }
        }
        let granule = offset & !15;
        if !self.in_bounds(granule, 16) {
            return None;
        }
        let shift = 8 * (offset - granule);
        // SAFETY: `granule` is 16-byte aligned and `granule + 16 <= len`.
        let (lo, hi) = unsafe {
            (
                AtomicU64::from_ptr(self.at(granule).cast()),
                AtomicU64::from_ptr(self.at(granule + 8).cast()),
            )
        };
        let cur = u128::from(u64::from_le(lo.load(Ordering::Acquire)))
            | (u128::from(u64::from_le(hi.load(Ordering::Acquire))) << 64);
        let old = ((cur >> shift) as u64) & mask;
        let next = (cur & !(u128::from(mask) << shift)) | (u128::from(f(old) & mask) << shift);
        lo.store((next as u64).to_le(), Ordering::Release);
        hi.store(((next >> 64) as u64).to_le(), Ordering::Release);
        Some(old)
    }

    /// 128-bit compare-and-swap (`amocas.q`) for 16-byte aligned guest RAM
    /// accesses. Returns the original `(lo, hi)` pair, or `None` if out of
    /// bounds or unaligned.
    #[inline]
    pub fn atomic_cas128(
        &self,
        offset: u64,
        expected: (u64, u64),
        new: (u64, u64),
    ) -> Option<(u64, u64)> {
        if offset & 15 != 0 || !self.in_bounds(offset, 16) {
            return None;
        }
        self.mark_dirty_range(offset, 16);
        let p = self.at(offset).cast::<u64>();
        // SAFETY: `self.ptr` is page-aligned (`mmap`), `offset` is 16-byte
        // aligned, and `offset + 16 <= self.len`.
        unsafe {
            let atom_lo = AtomicU64::from_ptr(p);
            let atom_hi = AtomicU64::from_ptr(p.add(1));
            let old_lo = u64::from_le(atom_lo.load(Ordering::Acquire));
            let old_hi = u64::from_le(atom_hi.load(Ordering::Acquire));
            if (old_lo, old_hi) == expected {
                atom_lo.store(new.0.to_le(), Ordering::Release);
                atom_hi.store(new.1.to_le(), Ordering::Release);
            }
            Some((old_lo, old_hi))
        }
    }
}

/// # Safety
/// `p` must be valid for reads of `size` bytes.
#[inline(always)]
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
#[inline(always)]
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

    #[test]
    fn atomic_compare_exchange_and_rmw() {
        let ram = Ram::new(32).unwrap();
        ram.write(0, AccessSize::Word, 10).unwrap();
        assert_eq!(
            ram.compare_exchange(0, AccessSize::Word, 9, 20),
            Some(false)
        );
        assert_eq!(
            ram.compare_exchange(0, AccessSize::Word, 10, 20),
            Some(true)
        );
        assert_eq!(ram.read(0, AccessSize::Word), Some(20));

        let old = ram.atomic_rmw(0, AccessSize::Word, |v| v + 5).unwrap();
        assert_eq!(old, 20);
        assert_eq!(ram.read(0, AccessSize::Word), Some(25));

        ram.write(8, AccessSize::Double, 0x1000_0000_0000).unwrap();
        let old64 = ram.atomic_rmw(8, AccessSize::Double, |v| v | 0xff).unwrap();
        assert_eq!(old64, 0x1000_0000_0000);
        assert_eq!(ram.read(8, AccessSize::Double), Some(0x1000_0000_00ff));
    }
}
