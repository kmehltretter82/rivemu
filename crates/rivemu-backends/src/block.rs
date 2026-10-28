//! Block device backends: raw files, in-memory disks and QCOW2 v2/v3 images.

use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};

/// Standard 512-byte sector size used by `virtio-blk`.
pub const SECTOR_SIZE: u64 = 512;

const QCOW2_MAGIC: u32 = 0x5146_49fb; // "QFI\xfb"
const QCOW_OFLAG_COPIED: u64 = 1 << 63;
const QCOW_OFLAG_COMPRESSED: u64 = 1 << 62;
const L2_OFFSET_MASK: u64 = 0x00ff_ffff_ffff_fe00;

/// Host storage backend for a block device.
pub trait BlockBackend: Send + Sync + fmt::Debug {
    /// Total disk size in 512-byte sectors.
    fn capacity_sectors(&self) -> u64;

    /// Whether writes are rejected.
    fn is_read_only(&self) -> bool;

    /// Reads `buf.len()` bytes starting at byte `offset`.
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()>;

    /// Writes `data` starting at byte `offset`.
    fn write_at(&self, offset: u64, data: &[u8]) -> io::Result<()>;

    /// Flushes buffered writes to stable storage.
    fn flush(&self) -> io::Result<()>;
}

/// Opens a disk image (`format` = `Some("raw")`, `Some("qcow2")` or `None` to auto-detect).
pub fn open_block_image(
    path: &Path,
    format: Option<&str>,
    read_only: bool,
) -> io::Result<Arc<dyn BlockBackend>> {
    let is_qcow2 = match format {
        Some("raw") => false,
        Some("qcow2") => true,
        None => {
            let mut f = File::open(path)?;
            let mut magic = [0u8; 4];
            f.read_exact(&mut magic).is_ok() && u32::from_be_bytes(magic) == QCOW2_MAGIC
        }
        Some(other) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("unsupported block format '{other}' (expected raw or qcow2)"),
            ));
        }
    };
    if is_qcow2 {
        Ok(Arc::new(Qcow2Block::open(path, read_only)?))
    } else {
        Ok(Arc::new(RawBlock::open(path, read_only)?))
    }
}

/// In-memory block backend (useful for tests and ephemeral disks).
#[derive(Debug)]
pub struct MemoryBlock {
    data: Mutex<Vec<u8>>,
    read_only: bool,
}

impl MemoryBlock {
    /// Creates a zero-initialised in-memory disk of `size` bytes (rounded up to 512 B).
    pub fn new(size: usize, read_only: bool) -> Arc<Self> {
        let aligned = size.div_ceil(SECTOR_SIZE as usize) * SECTOR_SIZE as usize;
        Arc::new(Self {
            data: Mutex::new(vec![0u8; aligned]),
            read_only,
        })
    }

    /// Creates an in-memory disk pre-populated with `bytes`.
    pub fn from_bytes(mut bytes: Vec<u8>, read_only: bool) -> Arc<Self> {
        let aligned = bytes.len().div_ceil(SECTOR_SIZE as usize) * SECTOR_SIZE as usize;
        bytes.resize(aligned, 0);
        Arc::new(Self {
            data: Mutex::new(bytes),
            read_only,
        })
    }
}

impl BlockBackend for MemoryBlock {
    fn capacity_sectors(&self) -> u64 {
        self.data.lock().unwrap().len() as u64 / SECTOR_SIZE
    }

    fn is_read_only(&self) -> bool {
        self.read_only
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        let d = self.data.lock().unwrap();
        let start = usize::try_from(offset).map_err(|_| io::ErrorKind::InvalidInput)?;
        let end = start
            .checked_add(buf.len())
            .ok_or(io::ErrorKind::InvalidInput)?;
        if end > d.len() {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "block read past end of disk",
            ));
        }
        buf.copy_from_slice(&d[start..end]);
        Ok(())
    }

    fn write_at(&self, offset: u64, data: &[u8]) -> io::Result<()> {
        if self.read_only {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "disk is read-only",
            ));
        }
        let mut d = self.data.lock().unwrap();
        let start = usize::try_from(offset).map_err(|_| io::ErrorKind::InvalidInput)?;
        let end = start
            .checked_add(data.len())
            .ok_or(io::ErrorKind::InvalidInput)?;
        if end > d.len() {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "block write past end of disk",
            ));
        }
        d[start..end].copy_from_slice(data);
        Ok(())
    }

    fn flush(&self) -> io::Result<()> {
        Ok(())
    }
}

/// Raw file-backed disk image.
#[derive(Debug)]
pub struct RawBlock {
    file: Mutex<File>,
    size: u64,
    read_only: bool,
}

impl RawBlock {
    /// Opens a raw disk image file.
    pub fn open(path: &Path, read_only: bool) -> io::Result<Self> {
        let file = OpenOptions::new().read(true).write(!read_only).open(path)?;
        let size = file.metadata()?.len();
        Ok(Self {
            file: Mutex::new(file),
            size,
            read_only,
        })
    }
}

impl BlockBackend for RawBlock {
    fn capacity_sectors(&self) -> u64 {
        self.size / SECTOR_SIZE
    }

    fn is_read_only(&self) -> bool {
        self.read_only
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        if offset
            .checked_add(buf.len() as u64)
            .is_none_or(|e| e > self.size)
        {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "raw block read out of bounds",
            ));
        }
        let mut f = self.file.lock().unwrap();
        f.seek(SeekFrom::Start(offset))?;
        f.read_exact(buf)
    }

    fn write_at(&self, offset: u64, data: &[u8]) -> io::Result<()> {
        if self.read_only {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "raw disk is read-only",
            ));
        }
        if offset
            .checked_add(data.len() as u64)
            .is_none_or(|e| e > self.size)
        {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "raw block write out of bounds",
            ));
        }
        let mut f = self.file.lock().unwrap();
        f.seek(SeekFrom::Start(offset))?;
        f.write_all(data)
    }

    fn flush(&self) -> io::Result<()> {
        self.file.lock().unwrap().sync_data()
    }
}

/// QCOW2 v2/v3 disk image backend (uncompressed read/write subset).
#[derive(Debug)]
pub struct Qcow2Block {
    state: Mutex<Qcow2State>,
    virtual_size: u64,
    cluster_bits: u32,
    read_only: bool,
}

#[derive(Debug)]
struct Qcow2State {
    file: File,
    l1_table_offset: u64,
    l1_table: Vec<u64>,
    next_alloc_offset: u64,
}

impl Qcow2Block {
    /// Opens an existing QCOW2 v2 or v3 image.
    pub fn open(path: &Path, read_only: bool) -> io::Result<Self> {
        let mut file = OpenOptions::new().read(true).write(!read_only).open(path)?;
        let mut hdr = [0u8; 72];
        file.read_exact(&mut hdr)?;
        let magic = u32::from_be_bytes(hdr[0..4].try_into().unwrap());
        if magic != QCOW2_MAGIC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "not a QCOW2 image (bad magic)",
            ));
        }
        let version = u32::from_be_bytes(hdr[4..8].try_into().unwrap());
        if version != 2 && version != 3 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unsupported QCOW2 version {version} (expected 2 or 3)"),
            ));
        }
        let backing_offset = u64::from_be_bytes(hdr[8..16].try_into().unwrap());
        if backing_offset != 0 {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "QCOW2 backing files are not supported",
            ));
        }
        let cluster_bits = u32::from_be_bytes(hdr[20..24].try_into().unwrap());
        if !(9..=21).contains(&cluster_bits) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid QCOW2 cluster_bits {cluster_bits}"),
            ));
        }
        let virtual_size = u64::from_be_bytes(hdr[24..32].try_into().unwrap());
        let crypt_method = u32::from_be_bytes(hdr[32..36].try_into().unwrap());
        if crypt_method != 0 {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "encrypted QCOW2 images are not supported",
            ));
        }
        let l1_size = u32::from_be_bytes(hdr[36..40].try_into().unwrap()) as usize;
        let l1_table_offset = u64::from_be_bytes(hdr[40..48].try_into().unwrap());

        file.seek(SeekFrom::Start(l1_table_offset))?;
        let mut l1_bytes = vec![0u8; l1_size * 8];
        file.read_exact(&mut l1_bytes)?;
        let l1_table: Vec<u64> = l1_bytes
            .chunks_exact(8)
            .map(|c| u64::from_be_bytes(c.try_into().unwrap()))
            .collect();

        let cluster_size = 1u64 << cluster_bits;
        let file_len = file.metadata()?.len();
        let next_alloc_offset = file_len.div_ceil(cluster_size) * cluster_size;

        Ok(Self {
            state: Mutex::new(Qcow2State {
                file,
                l1_table_offset,
                l1_table,
                next_alloc_offset,
            }),
            virtual_size,
            cluster_bits,
            read_only,
        })
    }

    /// Creates a new empty QCOW2 v3 image at `path` with `virtual_size` bytes.
    pub fn create(path: &Path, virtual_size: u64, cluster_bits: u32) -> io::Result<Self> {
        let cluster_size = 1u64 << cluster_bits;
        let l2_entries = cluster_size / 8;
        let total_clusters = virtual_size.div_ceil(cluster_size);
        let l1_size = total_clusters.div_ceil(l2_entries).max(1) as u32;
        // Layout:
        //   Cluster 0: header
        //   Cluster 1: refcount table
        //   Cluster 2: refcount block 0
        //   Cluster 3..: L1 table
        let refcount_table_offset = cluster_size;
        let refcount_block_offset = cluster_size * 2;
        let l1_table_offset = cluster_size * 3;
        let l1_clusters = (u64::from(l1_size) * 8).div_ceil(cluster_size);
        let total_file_size = (3 + l1_clusters) * cluster_size;

        let mut f = OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(path)?;
        f.set_len(total_file_size)?;

        let mut hdr = [0u8; 104];
        hdr[0..4].copy_from_slice(&QCOW2_MAGIC.to_be_bytes());
        hdr[4..8].copy_from_slice(&3u32.to_be_bytes()); // v3
        hdr[20..24].copy_from_slice(&cluster_bits.to_be_bytes());
        hdr[24..32].copy_from_slice(&virtual_size.to_be_bytes());
        hdr[36..40].copy_from_slice(&l1_size.to_be_bytes());
        hdr[40..48].copy_from_slice(&l1_table_offset.to_be_bytes());
        hdr[48..56].copy_from_slice(&refcount_table_offset.to_be_bytes());
        hdr[56..60].copy_from_slice(&1u32.to_be_bytes()); // refcount_table_clusters
        hdr[96..100].copy_from_slice(&4u32.to_be_bytes()); // refcount_order = 4 (16-bit)
        hdr[100..104].copy_from_slice(&104u32.to_be_bytes()); // header_length
        f.seek(SeekFrom::Start(0))?;
        f.write_all(&hdr)?;

        // Point refcount table[0] -> refcount_block_offset.
        f.seek(SeekFrom::Start(refcount_table_offset))?;
        f.write_all(&refcount_block_offset.to_be_bytes())?;
        f.sync_all()?;
        drop(f);

        Self::open(path, false)
    }

    #[inline]
    fn cluster_size(&self) -> u64 {
        1u64 << self.cluster_bits
    }

    fn read_chunk(&self, st: &mut Qcow2State, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        let csize = self.cluster_size();
        let l2_bits = self.cluster_bits - 3;
        let l2_mask = (1u64 << l2_bits) - 1;
        let cluster_idx = offset >> self.cluster_bits;
        let in_cluster = offset & (csize - 1);
        let l1_idx = (cluster_idx >> l2_bits) as usize;
        let l2_idx = cluster_idx & l2_mask;

        let l1_entry = st.l1_table.get(l1_idx).copied().unwrap_or(0);
        let l2_table_off = l1_entry & L2_OFFSET_MASK;
        if l2_table_off == 0 {
            buf.fill(0);
            return Ok(());
        }
        st.file.seek(SeekFrom::Start(l2_table_off + l2_idx * 8))?;
        let mut l2_raw = [0u8; 8];
        st.file.read_exact(&mut l2_raw)?;
        let l2_entry = u64::from_be_bytes(l2_raw);
        if l2_entry & QCOW_OFLAG_COMPRESSED != 0 {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "compressed QCOW2 clusters are not supported",
            ));
        }
        let data_cluster_off = l2_entry & L2_OFFSET_MASK;
        if data_cluster_off == 0 {
            buf.fill(0);
            return Ok(());
        }
        st.file
            .seek(SeekFrom::Start(data_cluster_off + in_cluster))?;
        st.file.read_exact(buf)
    }

    fn write_chunk(&self, st: &mut Qcow2State, offset: u64, data: &[u8]) -> io::Result<()> {
        let csize = self.cluster_size();
        let l2_bits = self.cluster_bits - 3;
        let l2_mask = (1u64 << l2_bits) - 1;
        let cluster_idx = offset >> self.cluster_bits;
        let in_cluster = offset & (csize - 1);
        let l1_idx = (cluster_idx >> l2_bits) as usize;
        let l2_idx = cluster_idx & l2_mask;

        if l1_idx >= st.l1_table.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "QCOW2 L1 index out of bounds",
            ));
        }
        let mut l2_table_off = st.l1_table[l1_idx] & L2_OFFSET_MASK;
        if l2_table_off == 0 {
            l2_table_off = st.next_alloc_offset;
            st.next_alloc_offset += csize;
            st.file.set_len(st.next_alloc_offset)?;
            let new_l1 = l2_table_off | QCOW_OFLAG_COPIED;
            st.l1_table[l1_idx] = new_l1;
            st.file
                .seek(SeekFrom::Start(st.l1_table_offset + l1_idx as u64 * 8))?;
            st.file.write_all(&new_l1.to_be_bytes())?;
        }

        let l2_entry_pos = l2_table_off + l2_idx * 8;
        st.file.seek(SeekFrom::Start(l2_entry_pos))?;
        let mut l2_raw = [0u8; 8];
        st.file.read_exact(&mut l2_raw)?;
        let l2_entry = u64::from_be_bytes(l2_raw);
        if l2_entry & QCOW_OFLAG_COMPRESSED != 0 {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "cannot overwrite compressed QCOW2 cluster",
            ));
        }
        let mut data_cluster_off = l2_entry & L2_OFFSET_MASK;
        if data_cluster_off == 0 {
            data_cluster_off = st.next_alloc_offset;
            st.next_alloc_offset += csize;
            st.file.set_len(st.next_alloc_offset)?;
            let new_l2 = data_cluster_off | QCOW_OFLAG_COPIED;
            st.file.seek(SeekFrom::Start(l2_entry_pos))?;
            st.file.write_all(&new_l2.to_be_bytes())?;
        }

        st.file
            .seek(SeekFrom::Start(data_cluster_off + in_cluster))?;
        st.file.write_all(data)
    }
}

impl BlockBackend for Qcow2Block {
    fn capacity_sectors(&self) -> u64 {
        self.virtual_size / SECTOR_SIZE
    }

    fn is_read_only(&self) -> bool {
        self.read_only
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        if offset
            .checked_add(buf.len() as u64)
            .is_none_or(|e| e > self.virtual_size)
        {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "QCOW2 read out of bounds",
            ));
        }
        let csize = self.cluster_size();
        let mut st = self.state.lock().unwrap();
        let mut done = 0usize;
        while done < buf.len() {
            let cur = offset + done as u64;
            let rem_in_cluster = (csize - (cur & (csize - 1))) as usize;
            let n = (buf.len() - done).min(rem_in_cluster);
            self.read_chunk(&mut st, cur, &mut buf[done..done + n])?;
            done += n;
        }
        Ok(())
    }

    fn write_at(&self, offset: u64, data: &[u8]) -> io::Result<()> {
        if self.read_only {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "QCOW2 image is read-only",
            ));
        }
        if offset
            .checked_add(data.len() as u64)
            .is_none_or(|e| e > self.virtual_size)
        {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "QCOW2 write out of bounds",
            ));
        }
        let csize = self.cluster_size();
        let mut st = self.state.lock().unwrap();
        let mut done = 0usize;
        while done < data.len() {
            let cur = offset + done as u64;
            let rem_in_cluster = (csize - (cur & (csize - 1))) as usize;
            let n = (data.len() - done).min(rem_in_cluster);
            self.write_chunk(&mut st, cur, &data[done..done + n])?;
            done += n;
        }
        Ok(())
    }

    fn flush(&self) -> io::Result<()> {
        self.state.lock().unwrap().file.sync_data()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_block_rw_and_ro() {
        let blk = MemoryBlock::new(4096, false);
        assert_eq!(blk.capacity_sectors(), 8);
        blk.write_at(500, &[1, 2, 3, 4]).unwrap();
        let mut buf = [0u8; 4];
        blk.read_at(500, &mut buf).unwrap();
        assert_eq!(buf, [1, 2, 3, 4]);
        assert!(blk.read_at(4094, &mut buf).is_err());

        let ro = MemoryBlock::new(512, true);
        assert!(ro.write_at(0, &[1]).is_err());
    }

    #[test]
    fn qcow2_create_read_write_persist() {
        let path = std::env::temp_dir().join(format!(
            "rivemu-qcow2-test-{}-{:?}.qcow2",
            std::process::id(),
            std::thread::current().id()
        ));
        let disk = Qcow2Block::create(&path, 1024 * 1024, 16).unwrap();
        assert_eq!(disk.capacity_sectors(), 2048);

        // Unallocated reads return zeros.
        let mut buf = [0xffu8; 16];
        disk.read_at(65530, &mut buf).unwrap();
        assert_eq!(buf, [0u8; 16]);

        // Cross-cluster write (cluster size = 64 KiB = 65536).
        let pattern: Vec<u8> = (0..32).map(|i| i ^ 0x5a).collect();
        disk.write_at(65520, &pattern).unwrap();
        disk.flush().unwrap();
        drop(disk);

        // Re-open with auto-detection and verify persistence.
        let reopened = open_block_image(&path, None, true).unwrap();
        let mut back = vec![0u8; 32];
        reopened.read_at(65520, &mut back).unwrap();
        assert_eq!(back, pattern);
        let _ = std::fs::remove_file(&path);
    }
}
