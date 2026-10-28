//! `virtio-9p-device` (`9P2000.L` host directory sharing over `virtio-mmio`, ID = 9).

use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};

use rivemu_core::AccessSize;

use crate::VirtioDevice;
use crate::queue::{DmaRam, VirtQueue};

const VIRTIO_ID_9P: u32 = 9;
const VIRTIO_9P_MOUNT_TAG: u64 = 1 << 0;

// 9P2000.L message types.
const RLERROR: u8 = 7;
const TSTATFS: u8 = 8;
const TLOPEN: u8 = 12;
const TLCREATE: u8 = 14;
const TGETATTR: u8 = 24;
const TSETATTR: u8 = 26;
const TREADDIR: u8 = 40;
const TFSYNC: u8 = 50;
const TMKDIR: u8 = 72;
const TUNLINKAT: u8 = 76;
const TVERSION: u8 = 100;
const TATTACH: u8 = 104;
const TFLUSH: u8 = 108;
const TWALK: u8 = 110;
const TREAD: u8 = 116;
const TWRITE: u8 = 118;
const TCLUNK: u8 = 120;

// Linux errno values used in Rlerror.
const ENOENT: u32 = 2;
const EIO: u32 = 5;
const EBADF: u32 = 9;
const EINVAL: u32 = 22;
const ENOSYS: u32 = 38;

/// QID type bits.
const QTDIR: u8 = 0x80;
const QTFILE: u8 = 0x00;

/// VirtIO 9P (`9P2000.L`) filesystem device exporting a host directory.
#[derive(Debug)]
pub struct Virtio9p {
    root: PathBuf,
    config: Vec<u8>,
    fids: HashMap<u32, PathBuf>,
}

impl Virtio9p {
    /// Creates a `virtio-9p-device` exporting `root` with the given `mount_tag`.
    pub fn new(root: impl Into<PathBuf>, mount_tag: impl AsRef<str>) -> Self {
        let tag = mount_tag.as_ref().as_bytes();
        let len = tag.len().min(u16::MAX as usize) as u16;
        let mut config = Vec::with_capacity(2 + usize::from(len));
        config.extend_from_slice(&len.to_le_bytes());
        config.extend_from_slice(&tag[..usize::from(len)]);
        Self {
            root: root.into(),
            config,
            fids: HashMap::new(),
        }
    }

    fn resolve_within_root(&self, base: &Path, name: &str) -> Result<PathBuf, u32> {
        if name.is_empty() || name == "." {
            return Ok(base.to_path_buf());
        }
        if name == ".." {
            if base == self.root {
                return Ok(self.root.clone());
            }
            return Ok(base.parent().unwrap_or(&self.root).to_path_buf());
        }
        let rel = Path::new(name);
        for c in rel.components() {
            if !matches!(c, Component::Normal(_)) {
                return Err(EINVAL);
            }
        }
        Ok(base.join(rel))
    }

    fn handle_message(&mut self, req: &[u8]) -> Vec<u8> {
        if req.len() < 7 {
            return build_error(0, EINVAL);
        }
        let msg_type = req[4];
        let tag = u16::from_le_bytes([req[5], req[6]]);
        let mut r = WireReader::new(&req[7..]);
        let mut w = WireWriter::new(msg_type.wrapping_add(1), tag);

        match self.dispatch(msg_type, &mut r, &mut w) {
            Ok(()) => w.finish(),
            Err(code) => build_error(tag, code),
        }
    }

    fn dispatch(
        &mut self,
        msg_type: u8,
        r: &mut WireReader<'_>,
        w: &mut WireWriter,
    ) -> Result<(), u32> {
        match msg_type {
            TVERSION => {
                let msize = r.u32().ok_or(EINVAL)?;
                let _ver = r.str().ok_or(EINVAL)?;
                w.u32(msize.min(65536));
                w.str("9P2000.L");
                Ok(())
            }
            TATTACH => {
                let fid = r.u32().ok_or(EINVAL)?;
                let _afid = r.u32().ok_or(EINVAL)?;
                let _uname = r.str().ok_or(EINVAL)?;
                let _aname = r.str().ok_or(EINVAL)?;
                let qid = path_qid(&self.root)?;
                self.fids.insert(fid, self.root.clone());
                w.qid(qid);
                Ok(())
            }
            TWALK => {
                let fid = r.u32().ok_or(EINVAL)?;
                let newfid = r.u32().ok_or(EINVAL)?;
                let nwname = r.u16().ok_or(EINVAL)?;
                let mut cur = self.fids.get(&fid).ok_or(EBADF)?.clone();
                let mut qids = Vec::with_capacity(usize::from(nwname));
                for _ in 0..nwname {
                    let name = r.str().ok_or(EINVAL)?;
                    let next = self.resolve_within_root(&cur, name)?;
                    match path_qid(&next) {
                        Ok(q) => {
                            cur = next;
                            qids.push(q);
                        }
                        Err(e) => {
                            if qids.is_empty() {
                                return Err(e);
                            }
                            break;
                        }
                    }
                }
                if qids.len() == usize::from(nwname) {
                    self.fids.insert(newfid, cur);
                }
                w.u16(qids.len() as u16);
                for q in qids {
                    w.qid(q);
                }
                Ok(())
            }
            TLOPEN => {
                let fid = r.u32().ok_or(EINVAL)?;
                let _flags = r.u32().ok_or(EINVAL)?;
                let path = self.fids.get(&fid).ok_or(EBADF)?;
                let qid = path_qid(path)?;
                w.qid(qid);
                w.u32(0); // iounit
                Ok(())
            }
            TLCREATE => {
                let fid = r.u32().ok_or(EINVAL)?;
                let name = r.str().ok_or(EINVAL)?;
                let _flags = r.u32().ok_or(EINVAL)?;
                let _mode = r.u32().ok_or(EINVAL)?;
                let _gid = r.u32().ok_or(EINVAL)?;
                let dir = self.fids.get(&fid).ok_or(EBADF)?.clone();
                let target = self.resolve_within_root(&dir, name)?;
                fs::File::create(&target).map_err(io_errno)?;
                let qid = path_qid(&target)?;
                self.fids.insert(fid, target);
                w.qid(qid);
                w.u32(0);
                Ok(())
            }
            TGETATTR => {
                let fid = r.u32().ok_or(EINVAL)?;
                let _mask = r.u64().ok_or(EINVAL)?;
                let path = self.fids.get(&fid).ok_or(EBADF)?;
                let meta = fs::symlink_metadata(path).map_err(io_errno)?;
                let qid = meta_qid(&meta);
                let mode: u32 = if meta.is_dir() { 0o040_755 } else { 0o100_755 };
                let size = meta.len();
                w.u64(0x07ff); // P9_STATS_BASIC
                w.qid(qid);
                w.u32(mode);
                w.u32(0); // uid
                w.u32(0); // gid
                w.u64(1); // nlink
                w.u64(0); // rdev
                w.u64(size);
                w.u64(4096); // blksize
                w.u64(size.div_ceil(512)); // blocks
                for _ in 0..10 {
                    w.u64(0); // atime/mtime/ctime/btime/gen/data_version
                }
                Ok(())
            }
            TSETATTR => {
                let fid = r.u32().ok_or(EINVAL)?;
                let valid = r.u32().ok_or(EINVAL)?;
                let _mode = r.u32().ok_or(EINVAL)?;
                let _uid = r.u32().ok_or(EINVAL)?;
                let _gid = r.u32().ok_or(EINVAL)?;
                let size = r.u64().ok_or(EINVAL)?;
                let path = self.fids.get(&fid).ok_or(EBADF)?;
                if valid & (1 << 3) != 0 {
                    let f = OpenOptions::new()
                        .write(true)
                        .open(path)
                        .map_err(io_errno)?;
                    f.set_len(size).map_err(io_errno)?;
                }
                Ok(())
            }
            TREADDIR => {
                let fid = r.u32().ok_or(EINVAL)?;
                let offset = r.u64().ok_or(EINVAL)?;
                let count = r.u32().ok_or(EINVAL)? as usize;
                let path = self.fids.get(&fid).ok_or(EBADF)?;
                let mut entries: Vec<(String, PathBuf)> = Vec::new();
                for ent in fs::read_dir(path).map_err(io_errno)? {
                    let ent = ent.map_err(io_errno)?;
                    if let Ok(name) = ent.file_name().into_string() {
                        entries.push((name, ent.path()));
                    }
                }
                entries.sort_by(|a, b| a.0.cmp(&b.0));
                let mut payload = Vec::new();
                for (idx, (name, p)) in entries.into_iter().enumerate().skip(offset as usize) {
                    let Ok(meta) = fs::symlink_metadata(&p) else {
                        continue;
                    };
                    let qid = meta_qid(&meta);
                    let dtype: u8 = if meta.is_dir() { 4 } else { 8 };
                    let entry_len = 13 + 8 + 1 + 2 + name.len();
                    if payload.len() + entry_len > count {
                        break;
                    }
                    payload.push(qid.0);
                    payload.extend_from_slice(&qid.1.to_le_bytes());
                    payload.extend_from_slice(&qid.2.to_le_bytes());
                    payload.extend_from_slice(&((idx + 1) as u64).to_le_bytes());
                    payload.push(dtype);
                    payload.extend_from_slice(&(name.len() as u16).to_le_bytes());
                    payload.extend_from_slice(name.as_bytes());
                }
                w.u32(payload.len() as u32);
                w.bytes(&payload);
                Ok(())
            }
            TREAD => {
                let fid = r.u32().ok_or(EINVAL)?;
                let offset = r.u64().ok_or(EINVAL)?;
                let count = r.u32().ok_or(EINVAL)?.min(65536) as usize;
                let path = self.fids.get(&fid).ok_or(EBADF)?;
                let mut f = fs::File::open(path).map_err(io_errno)?;
                f.seek(SeekFrom::Start(offset)).map_err(io_errno)?;
                let mut buf = vec![0u8; count];
                let n = f.read(&mut buf).map_err(io_errno)?;
                w.u32(n as u32);
                w.bytes(&buf[..n]);
                Ok(())
            }
            TWRITE => {
                let fid = r.u32().ok_or(EINVAL)?;
                let offset = r.u64().ok_or(EINVAL)?;
                let count = r.u32().ok_or(EINVAL)? as usize;
                let data = r.bytes(count).ok_or(EINVAL)?;
                let path = self.fids.get(&fid).ok_or(EBADF)?;
                let mut f = OpenOptions::new()
                    .write(true)
                    .open(path)
                    .map_err(io_errno)?;
                f.seek(SeekFrom::Start(offset)).map_err(io_errno)?;
                f.write_all(data).map_err(io_errno)?;
                w.u32(count as u32);
                Ok(())
            }
            TMKDIR => {
                let dfid = r.u32().ok_or(EINVAL)?;
                let name = r.str().ok_or(EINVAL)?;
                let _mode = r.u32().ok_or(EINVAL)?;
                let _gid = r.u32().ok_or(EINVAL)?;
                let dir = self.fids.get(&dfid).ok_or(EBADF)?.clone();
                let target = self.resolve_within_root(&dir, name)?;
                fs::create_dir(&target).map_err(io_errno)?;
                let qid = path_qid(&target)?;
                w.qid(qid);
                Ok(())
            }
            TUNLINKAT => {
                let dfid = r.u32().ok_or(EINVAL)?;
                let name = r.str().ok_or(EINVAL)?;
                let _flags = r.u32().ok_or(EINVAL)?;
                let dir = self.fids.get(&dfid).ok_or(EBADF)?.clone();
                let target = self.resolve_within_root(&dir, name)?;
                let meta = fs::symlink_metadata(&target).map_err(io_errno)?;
                if meta.is_dir() {
                    fs::remove_dir(&target).map_err(io_errno)?;
                } else {
                    fs::remove_file(&target).map_err(io_errno)?;
                }
                Ok(())
            }
            TSTATFS => {
                let _fid = r.u32().ok_or(EINVAL)?;
                w.u32(0x0102_1997); // V9FS_MAGIC
                w.u32(4096); // bsize
                w.u64(1_000_000); // blocks
                w.u64(500_000); // bfree
                w.u64(500_000); // bavail
                w.u64(100_000); // files
                w.u64(90_000); // ffree
                w.u64(0); // fsid
                w.u32(255); // namelen
                Ok(())
            }
            TFSYNC | TFLUSH => Ok(()),
            TCLUNK => {
                let fid = r.u32().ok_or(EINVAL)?;
                self.fids.remove(&fid);
                Ok(())
            }
            _ => Err(ENOSYS),
        }
    }
}

fn io_errno(err: std::io::Error) -> u32 {
    use std::io::ErrorKind;
    match err.kind() {
        ErrorKind::NotFound => ENOENT,
        ErrorKind::InvalidInput => EINVAL,
        _ => EIO,
    }
}

fn path_qid(path: &Path) -> Result<(u8, u32, u64), u32> {
    let meta = fs::symlink_metadata(path).map_err(io_errno)?;
    Ok(meta_qid(&meta))
}

#[cfg(unix)]
fn meta_qid(meta: &fs::Metadata) -> (u8, u32, u64) {
    use std::os::unix::fs::MetadataExt;
    let qtype = if meta.is_dir() { QTDIR } else { QTFILE };
    (qtype, 0, meta.ino())
}

#[cfg(not(unix))]
fn meta_qid(meta: &fs::Metadata) -> (u8, u32, u64) {
    let qtype = if meta.is_dir() { QTDIR } else { QTFILE };
    (qtype, 0, meta.len() ^ 0x9p00_0001)
}

fn build_error(tag: u16, ecode: u32) -> Vec<u8> {
    let mut w = WireWriter::new(RLERROR, tag);
    w.u32(ecode);
    w.finish()
}

struct WireReader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> WireReader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn bytes(&mut self, len: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(len)?;
        let s = self.buf.get(self.pos..end)?;
        self.pos = end;
        Some(s)
    }

    fn u16(&mut self) -> Option<u16> {
        Some(u16::from_le_bytes(self.bytes(2)?.try_into().ok()?))
    }

    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.bytes(4)?.try_into().ok()?))
    }

    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.bytes(8)?.try_into().ok()?))
    }

    fn str(&mut self) -> Option<&'a str> {
        let len = usize::from(self.u16()?);
        std::str::from_utf8(self.bytes(len)?).ok()
    }
}

struct WireWriter {
    buf: Vec<u8>,
}

impl WireWriter {
    fn new(msg_type: u8, tag: u16) -> Self {
        let mut buf = Vec::with_capacity(64);
        buf.extend_from_slice(&0u32.to_le_bytes()); // size placeholder
        buf.push(msg_type);
        buf.extend_from_slice(&tag.to_le_bytes());
        Self { buf }
    }

    fn u16(&mut self, v: u16) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    fn u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    fn u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    fn bytes(&mut self, b: &[u8]) {
        self.buf.extend_from_slice(b);
    }

    fn str(&mut self, s: &str) {
        self.u16(s.len() as u16);
        self.bytes(s.as_bytes());
    }

    fn qid(&mut self, (qtype, version, path): (u8, u32, u64)) {
        self.buf.push(qtype);
        self.u32(version);
        self.u64(path);
    }

    fn finish(mut self) -> Vec<u8> {
        let len = self.buf.len() as u32;
        self.buf[0..4].copy_from_slice(&len.to_le_bytes());
        self.buf
    }
}

impl VirtioDevice for Virtio9p {
    fn device_id(&self) -> u32 {
        VIRTIO_ID_9P
    }

    fn num_queues(&self) -> usize {
        1
    }

    fn device_features(&self) -> u64 {
        VIRTIO_9P_MOUNT_TAG
    }

    fn read_config(&self, offset: u64, size: AccessSize) -> u64 {
        let off = offset as usize;
        let n = size.bytes() as usize;
        let mut val = 0u64;
        for i in 0..n {
            if let Some(&b) = self.config.get(off + i) {
                val |= u64::from(b) << (i * 8);
            }
        }
        val
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
            let Some(req) = chain.read_all(dma) else {
                continue;
            };
            let resp = self.handle_message(&req);
            let written = chain.write_all(dma, &resp).unwrap_or(0);
            queue.push_used(dma, chain.head, written, event_idx);
            completed += 1;
        }
        completed
    }
}
