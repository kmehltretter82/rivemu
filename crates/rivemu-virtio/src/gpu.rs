//! `virtio-gpu` (VirtIO Device ID 16) 2D scanout GPU and `Framebuffer` export (PPM / PNG).

use std::collections::HashMap;
use std::io;
use std::path::Path;
use std::sync::{Arc, Mutex};

use rivemu_core::AccessSize;

use crate::{DmaRam, VirtQueue, VirtioDevice};

/// Default scanout width in pixels.
pub const DEFAULT_GPU_WIDTH: u32 = 640;
/// Default scanout height in pixels.
pub const DEFAULT_GPU_HEIGHT: u32 = 480;

/// VirtIO GPU feature bit: EDID blob query (`VIRTIO_GPU_CMD_GET_EDID`).
pub const VIRTIO_GPU_F_EDID: u64 = 1 << 1;

// 2D Controlq commands (`enum virtio_gpu_ctrl_type`).
const VIRTIO_GPU_CMD_GET_DISPLAY_INFO: u32 = 0x0101;
const VIRTIO_GPU_CMD_RESOURCE_CREATE_2D: u32 = 0x0102;
const VIRTIO_GPU_CMD_RESOURCE_UNREF: u32 = 0x0103;
const VIRTIO_GPU_CMD_SET_SCANOUT: u32 = 0x0104;
const VIRTIO_GPU_CMD_RESOURCE_FLUSH: u32 = 0x0105;
const VIRTIO_GPU_CMD_TRANSFER_TO_HOST_2D: u32 = 0x0106;
const VIRTIO_GPU_CMD_RESOURCE_ATTACH_BACKING: u32 = 0x0107;
const VIRTIO_GPU_CMD_RESOURCE_DETACH_BACKING: u32 = 0x0108;
const VIRTIO_GPU_CMD_GET_EDID: u32 = 0x010a;

// Cursorq commands.
const VIRTIO_GPU_CMD_UPDATE_CURSOR: u32 = 0x0300;
const VIRTIO_GPU_CMD_MOVE_CURSOR: u32 = 0x0301;

// Responses.
const VIRTIO_GPU_RESP_OK_NODATA: u32 = 0x1100;
const VIRTIO_GPU_RESP_OK_DISPLAY_INFO: u32 = 0x1101;
const VIRTIO_GPU_RESP_OK_EDID: u32 = 0x1104;
const VIRTIO_GPU_RESP_ERR_UNSPEC: u32 = 0x1200;
const VIRTIO_GPU_RESP_ERR_INVALID_RESOURCE_ID: u32 = 0x1202;
const VIRTIO_GPU_RESP_ERR_INVALID_PARAMETER: u32 = 0x1203;

const VIRTIO_GPU_FLAG_FENCE: u32 = 1 << 0;
const VIRTIO_GPU_MAX_SCANOUTS: usize = 16;

// Pixel formats (`enum virtio_gpu_formats`).
const VIRTIO_GPU_FORMAT_B8G8R8A8_UNORM: u32 = 1;
const VIRTIO_GPU_FORMAT_B8G8R8X8_UNORM: u32 = 2;
const VIRTIO_GPU_FORMAT_A8R8G8B8_UNORM: u32 = 3;
const VIRTIO_GPU_FORMAT_X8R8G8B8_UNORM: u32 = 4;
const VIRTIO_GPU_FORMAT_R8G8B8A8_UNORM: u32 = 67;
const VIRTIO_GPU_FORMAT_X8B8G8R8_UNORM: u32 = 68;
const VIRTIO_GPU_FORMAT_A8B8G8R8_UNORM: u32 = 121;
const VIRTIO_GPU_FORMAT_R8G8B8X8_UNORM: u32 = 134;

#[derive(Debug)]
struct FramebufferInner {
    width: u32,
    height: u32,
    /// Tightly packed `[R, G, B, A]` pixels (`width * height * 4` bytes).
    rgba: Vec<u8>,
    /// Incremented every time the scanout is flushed.
    generation: u64,
}

/// Shared host framebuffer receiving 2D scanouts from [`VirtioGpu`].
#[derive(Debug)]
pub struct Framebuffer {
    inner: Mutex<FramebufferInner>,
}

impl Default for Framebuffer {
    fn default() -> Self {
        Self::new(DEFAULT_GPU_WIDTH, DEFAULT_GPU_HEIGHT)
    }
}

impl Framebuffer {
    /// Creates a new black RGBA8888 framebuffer of `width x height` pixels.
    pub fn new(width: u32, height: u32) -> Self {
        let w = width.clamp(16, 4096);
        let h = height.clamp(16, 4096);
        let mut rgba = vec![0u8; (w as usize) * (h as usize) * 4];
        for px in rgba.chunks_exact_mut(4) {
            px[3] = 0xff;
        }
        Self {
            inner: Mutex::new(FramebufferInner {
                width: w,
                height: h,
                rgba,
                generation: 0,
            }),
        }
    }

    /// Returns `(width, height, generation)`.
    pub fn dimensions(&self) -> (u32, u32, u64) {
        let fb = self.inner.lock().unwrap();
        (fb.width, fb.height, fb.generation)
    }

    /// Returns a snapshot of `(width, height, rgba_bytes, generation)`.
    pub fn snapshot_rgba(&self) -> (u32, u32, Vec<u8>, u64) {
        let fb = self.inner.lock().unwrap();
        (fb.width, fb.height, fb.rgba.clone(), fb.generation)
    }

    /// Replaces the scanout buffer with `rgba` (`width * height * 4` bytes).
    pub fn update(&self, width: u32, height: u32, rgba: &[u8]) {
        let expected = (width as usize) * (height as usize) * 4;
        if rgba.len() != expected {
            return;
        }
        let mut fb = self.inner.lock().unwrap();
        fb.width = width;
        fb.height = height;
        fb.rgba.clear();
        fb.rgba.extend_from_slice(rgba);
        fb.generation = fb.generation.wrapping_add(1);
    }

    /// Encodes the current framebuffer as a binary PPM (`P6`) image.
    pub fn to_ppm(&self) -> Vec<u8> {
        let fb = self.inner.lock().unwrap();
        let header = format!("P6\n{} {}\n255\n", fb.width, fb.height);
        let mut out =
            Vec::with_capacity(header.len() + (fb.width as usize) * (fb.height as usize) * 3);
        out.extend_from_slice(header.as_bytes());
        for px in fb.rgba.chunks_exact(4) {
            out.push(px[0]);
            out.push(px[1]);
            out.push(px[2]);
        }
        out
    }

    /// Encodes the current framebuffer as a standard lossless 24-bit RGB PNG image.
    pub fn to_png(&self) -> Vec<u8> {
        let fb = self.inner.lock().unwrap();
        encode_rgb_png(fb.width, fb.height, &fb.rgba)
    }

    /// Writes the current framebuffer to `path` (`.png` if the extension is `.png`,
    /// otherwise `.ppm`).
    pub fn save_screendump(&self, path: &Path) -> io::Result<()> {
        let is_png = path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("png"));
        let bytes = if is_png { self.to_png() } else { self.to_ppm() };
        std::fs::write(path, bytes)
    }
}

#[derive(Clone, Debug)]
struct GpuResource2D {
    width: u32,
    height: u32,
    format: u32,
    /// Host RGBA8888 pixel storage (`width * height * 4` bytes).
    rgba: Vec<u8>,
    /// Guest scatter-gather backing segments `(gpa, len)`.
    backing: Vec<(u64, u32)>,
}

impl GpuResource2D {
    fn new(width: u32, height: u32, format: u32) -> Option<Self> {
        if width == 0 || height == 0 || width > 4096 || height > 4096 {
            return None;
        }
        let mut rgba = vec![0u8; (width as usize) * (height as usize) * 4];
        for px in rgba.chunks_exact_mut(4) {
            px[3] = 0xff;
        }
        Some(Self {
            width,
            height,
            format,
            rgba,
            backing: Vec::new(),
        })
    }

    /// Reads `buf.len()` bytes from the attached guest scatter-gather backing
    /// starting at byte `offset`.
    fn read_backing(&self, dma: &DmaRam, mut offset: u64, mut dst: &mut [u8]) -> bool {
        for &(seg_addr, seg_len) in &self.backing {
            if dst.is_empty() {
                return true;
            }
            let seg_len_u64 = u64::from(seg_len);
            if offset >= seg_len_u64 {
                offset -= seg_len_u64;
                continue;
            }
            let avail = (seg_len_u64 - offset) as usize;
            let take = avail.min(dst.len());
            if dma
                .read_bytes(seg_addr + offset, &mut dst[..take])
                .is_none()
            {
                return false;
            }
            dst = &mut dst[take..];
            offset = 0;
        }
        dst.is_empty()
    }
}

/// Converts a 4-byte pixel from `format` into `[R, G, B, A]`.
#[inline]
fn pixel_to_rgba(format: u32, src: [u8; 4]) -> [u8; 4] {
    match format {
        VIRTIO_GPU_FORMAT_B8G8R8A8_UNORM => [src[2], src[1], src[0], src[3]],
        VIRTIO_GPU_FORMAT_B8G8R8X8_UNORM => [src[2], src[1], src[0], 0xff],
        VIRTIO_GPU_FORMAT_A8R8G8B8_UNORM => [src[1], src[2], src[3], src[0]],
        VIRTIO_GPU_FORMAT_X8R8G8B8_UNORM => [src[1], src[2], src[3], 0xff],
        VIRTIO_GPU_FORMAT_R8G8B8A8_UNORM => src,
        VIRTIO_GPU_FORMAT_R8G8B8X8_UNORM => [src[0], src[1], src[2], 0xff],
        VIRTIO_GPU_FORMAT_X8B8G8R8_UNORM => [src[3], src[2], src[1], 0xff],
        VIRTIO_GPU_FORMAT_A8B8G8R8_UNORM => [src[3], src[2], src[1], src[0]],
        _ => [src[2], src[1], src[0], 0xff],
    }
}

/// VirtIO 2D GPU (`virtio-gpu-device` / `virtio-gpu-pci`, Device ID 16).
#[derive(Debug)]
pub struct VirtioGpu {
    width: u32,
    height: u32,
    events_read: u32,
    scanout_resource_id: u32,
    cursor_x: u32,
    cursor_y: u32,
    cursor_resource_id: u32,
    resources: HashMap<u32, GpuResource2D>,
    fb: Arc<Framebuffer>,
}

impl Default for VirtioGpu {
    fn default() -> Self {
        let fb = Arc::new(Framebuffer::default());
        Self::new(DEFAULT_GPU_WIDTH, DEFAULT_GPU_HEIGHT, fb)
    }
}

impl VirtioGpu {
    /// Creates a new `VirtioGpu` with preferred scanout resolution `width x height`
    /// backed by `fb`.
    pub fn new(width: u32, height: u32, fb: Arc<Framebuffer>) -> Self {
        let w = width.clamp(16, 4096);
        let h = height.clamp(16, 4096);
        Self {
            width: w,
            height: h,
            events_read: 0,
            scanout_resource_id: 0,
            cursor_x: 0,
            cursor_y: 0,
            cursor_resource_id: 0,
            resources: HashMap::new(),
            fb,
        }
    }

    /// Returns the shared host [`Framebuffer`].
    pub fn framebuffer(&self) -> Arc<Framebuffer> {
        self.fb.clone()
    }

    fn build_hdr(resp_type: u32, req_flags: u32, fence_id: u64) -> [u8; 24] {
        let mut hdr = [0u8; 24];
        hdr[0..4].copy_from_slice(&resp_type.to_le_bytes());
        if req_flags & VIRTIO_GPU_FLAG_FENCE != 0 {
            hdr[4..8].copy_from_slice(&VIRTIO_GPU_FLAG_FENCE.to_le_bytes());
            hdr[8..16].copy_from_slice(&fence_id.to_le_bytes());
        }
        hdr
    }

    fn handle_ctrl_cmd(&mut self, req: &[u8], dma: &DmaRam) -> Vec<u8> {
        if req.len() < 24 {
            return Self::build_hdr(VIRTIO_GPU_RESP_ERR_UNSPEC, 0, 0).to_vec();
        }
        let cmd_type = u32::from_le_bytes(req[0..4].try_into().unwrap());
        let flags = u32::from_le_bytes(req[4..8].try_into().unwrap());
        let fence_id = u64::from_le_bytes(req[8..16].try_into().unwrap());

        match cmd_type {
            VIRTIO_GPU_CMD_GET_DISPLAY_INFO => {
                let mut resp = vec![0u8; 24 + VIRTIO_GPU_MAX_SCANOUTS * 24];
                resp[0..24].copy_from_slice(&Self::build_hdr(
                    VIRTIO_GPU_RESP_OK_DISPLAY_INFO,
                    flags,
                    fence_id,
                ));
                // Scanout 0: x=0, y=0, width, height, enabled=1, flags=0.
                resp[32..36].copy_from_slice(&self.width.to_le_bytes());
                resp[36..40].copy_from_slice(&self.height.to_le_bytes());
                resp[40..44].copy_from_slice(&1u32.to_le_bytes());
                resp
            }
            VIRTIO_GPU_CMD_GET_EDID => {
                let mut resp = vec![0u8; 24 + 8 + 1024];
                resp[0..24].copy_from_slice(&Self::build_hdr(
                    VIRTIO_GPU_RESP_OK_EDID,
                    flags,
                    fence_id,
                ));
                let edid = build_edid(self.width, self.height);
                resp[24..28].copy_from_slice(&(edid.len() as u32).to_le_bytes());
                resp[32..32 + edid.len()].copy_from_slice(&edid);
                resp
            }
            VIRTIO_GPU_CMD_RESOURCE_CREATE_2D => {
                if req.len() < 40 {
                    return Self::build_hdr(VIRTIO_GPU_RESP_ERR_INVALID_PARAMETER, flags, fence_id)
                        .to_vec();
                }
                let res_id = u32::from_le_bytes(req[24..28].try_into().unwrap());
                let format = u32::from_le_bytes(req[28..32].try_into().unwrap());
                let width = u32::from_le_bytes(req[32..36].try_into().unwrap());
                let height = u32::from_le_bytes(req[36..40].try_into().unwrap());
                if res_id == 0 {
                    return Self::build_hdr(
                        VIRTIO_GPU_RESP_ERR_INVALID_RESOURCE_ID,
                        flags,
                        fence_id,
                    )
                    .to_vec();
                }
                let Some(res) = GpuResource2D::new(width, height, format) else {
                    return Self::build_hdr(VIRTIO_GPU_RESP_ERR_INVALID_PARAMETER, flags, fence_id)
                        .to_vec();
                };
                self.resources.insert(res_id, res);
                Self::build_hdr(VIRTIO_GPU_RESP_OK_NODATA, flags, fence_id).to_vec()
            }
            VIRTIO_GPU_CMD_RESOURCE_UNREF => {
                if req.len() < 32 {
                    return Self::build_hdr(VIRTIO_GPU_RESP_ERR_INVALID_PARAMETER, flags, fence_id)
                        .to_vec();
                }
                let res_id = u32::from_le_bytes(req[24..28].try_into().unwrap());
                if self.resources.remove(&res_id).is_none() {
                    return Self::build_hdr(
                        VIRTIO_GPU_RESP_ERR_INVALID_RESOURCE_ID,
                        flags,
                        fence_id,
                    )
                    .to_vec();
                }
                if self.scanout_resource_id == res_id {
                    self.scanout_resource_id = 0;
                }
                Self::build_hdr(VIRTIO_GPU_RESP_OK_NODATA, flags, fence_id).to_vec()
            }
            VIRTIO_GPU_CMD_RESOURCE_ATTACH_BACKING => {
                if req.len() < 32 {
                    return Self::build_hdr(VIRTIO_GPU_RESP_ERR_INVALID_PARAMETER, flags, fence_id)
                        .to_vec();
                }
                let res_id = u32::from_le_bytes(req[24..28].try_into().unwrap());
                let nr_entries = u32::from_le_bytes(req[28..32].try_into().unwrap()) as usize;
                if req.len() < 32 + nr_entries * 16 {
                    return Self::build_hdr(VIRTIO_GPU_RESP_ERR_INVALID_PARAMETER, flags, fence_id)
                        .to_vec();
                }
                let Some(res) = self.resources.get_mut(&res_id) else {
                    return Self::build_hdr(
                        VIRTIO_GPU_RESP_ERR_INVALID_RESOURCE_ID,
                        flags,
                        fence_id,
                    )
                    .to_vec();
                };
                res.backing.clear();
                for i in 0..nr_entries {
                    let base = 32 + i * 16;
                    let addr = u64::from_le_bytes(req[base..base + 8].try_into().unwrap());
                    let length = u32::from_le_bytes(req[base + 8..base + 12].try_into().unwrap());
                    res.backing.push((addr, length));
                }
                Self::build_hdr(VIRTIO_GPU_RESP_OK_NODATA, flags, fence_id).to_vec()
            }
            VIRTIO_GPU_CMD_RESOURCE_DETACH_BACKING => {
                if req.len() < 32 {
                    return Self::build_hdr(VIRTIO_GPU_RESP_ERR_INVALID_PARAMETER, flags, fence_id)
                        .to_vec();
                }
                let res_id = u32::from_le_bytes(req[24..28].try_into().unwrap());
                let Some(res) = self.resources.get_mut(&res_id) else {
                    return Self::build_hdr(
                        VIRTIO_GPU_RESP_ERR_INVALID_RESOURCE_ID,
                        flags,
                        fence_id,
                    )
                    .to_vec();
                };
                res.backing.clear();
                Self::build_hdr(VIRTIO_GPU_RESP_OK_NODATA, flags, fence_id).to_vec()
            }
            VIRTIO_GPU_CMD_SET_SCANOUT => {
                if req.len() < 48 {
                    return Self::build_hdr(VIRTIO_GPU_RESP_ERR_INVALID_PARAMETER, flags, fence_id)
                        .to_vec();
                }
                let scanout_id = u32::from_le_bytes(req[40..44].try_into().unwrap());
                let res_id = u32::from_le_bytes(req[44..48].try_into().unwrap());
                if scanout_id != 0 {
                    return Self::build_hdr(VIRTIO_GPU_RESP_ERR_INVALID_PARAMETER, flags, fence_id)
                        .to_vec();
                }
                if res_id != 0 && !self.resources.contains_key(&res_id) {
                    return Self::build_hdr(
                        VIRTIO_GPU_RESP_ERR_INVALID_RESOURCE_ID,
                        flags,
                        fence_id,
                    )
                    .to_vec();
                }
                self.scanout_resource_id = res_id;
                Self::build_hdr(VIRTIO_GPU_RESP_OK_NODATA, flags, fence_id).to_vec()
            }
            VIRTIO_GPU_CMD_TRANSFER_TO_HOST_2D => {
                if req.len() < 56 {
                    return Self::build_hdr(VIRTIO_GPU_RESP_ERR_INVALID_PARAMETER, flags, fence_id)
                        .to_vec();
                }
                let rx = u32::from_le_bytes(req[24..28].try_into().unwrap());
                let ry = u32::from_le_bytes(req[28..32].try_into().unwrap());
                let rw = u32::from_le_bytes(req[32..36].try_into().unwrap());
                let rh = u32::from_le_bytes(req[36..40].try_into().unwrap());
                let offset = u64::from_le_bytes(req[40..48].try_into().unwrap());
                let res_id = u32::from_le_bytes(req[48..52].try_into().unwrap());
                let Some(res) = self.resources.get_mut(&res_id) else {
                    return Self::build_hdr(
                        VIRTIO_GPU_RESP_ERR_INVALID_RESOURCE_ID,
                        flags,
                        fence_id,
                    )
                    .to_vec();
                };
                if rx.saturating_add(rw) > res.width || ry.saturating_add(rh) > res.height {
                    return Self::build_hdr(VIRTIO_GPU_RESP_ERR_INVALID_PARAMETER, flags, fence_id)
                        .to_vec();
                }
                let stride = u64::from(res.width) * 4;
                let mut row_buf = vec![0u8; (rw as usize) * 4];
                let fmt = res.format;
                for row in 0..rh {
                    let src_off = offset + u64::from(row) * stride;
                    if !res.read_backing(dma, src_off, &mut row_buf) {
                        break;
                    }
                    let dst_y = (ry + row) as usize;
                    for col in 0..rw as usize {
                        let px = [
                            row_buf[col * 4],
                            row_buf[col * 4 + 1],
                            row_buf[col * 4 + 2],
                            row_buf[col * 4 + 3],
                        ];
                        let rgba = pixel_to_rgba(fmt, px);
                        let dst_idx = (dst_y * (res.width as usize) + (rx as usize) + col) * 4;
                        res.rgba[dst_idx..dst_idx + 4].copy_from_slice(&rgba);
                    }
                }
                Self::build_hdr(VIRTIO_GPU_RESP_OK_NODATA, flags, fence_id).to_vec()
            }
            VIRTIO_GPU_CMD_RESOURCE_FLUSH => {
                if req.len() < 48 {
                    return Self::build_hdr(VIRTIO_GPU_RESP_ERR_INVALID_PARAMETER, flags, fence_id)
                        .to_vec();
                }
                let res_id = u32::from_le_bytes(req[40..44].try_into().unwrap());
                let Some(res) = self.resources.get(&res_id) else {
                    return Self::build_hdr(
                        VIRTIO_GPU_RESP_ERR_INVALID_RESOURCE_ID,
                        flags,
                        fence_id,
                    )
                    .to_vec();
                };
                if self.scanout_resource_id == res_id || self.scanout_resource_id == 0 {
                    self.fb.update(res.width, res.height, &res.rgba);
                }
                Self::build_hdr(VIRTIO_GPU_RESP_OK_NODATA, flags, fence_id).to_vec()
            }
            _ => Self::build_hdr(VIRTIO_GPU_RESP_ERR_UNSPEC, flags, fence_id).to_vec(),
        }
    }

    fn handle_cursor_cmd(&mut self, req: &[u8]) {
        if req.len() < 56 {
            return;
        }
        let cmd_type = u32::from_le_bytes(req[0..4].try_into().unwrap());
        let x = u32::from_le_bytes(req[28..32].try_into().unwrap());
        let y = u32::from_le_bytes(req[32..36].try_into().unwrap());
        let res_id = u32::from_le_bytes(req[36..40].try_into().unwrap());
        match cmd_type {
            VIRTIO_GPU_CMD_UPDATE_CURSOR => {
                self.cursor_x = x;
                self.cursor_y = y;
                self.cursor_resource_id = res_id;
            }
            VIRTIO_GPU_CMD_MOVE_CURSOR => {
                self.cursor_x = x;
                self.cursor_y = y;
            }
            _ => {}
        }
    }
}

impl VirtioDevice for VirtioGpu {
    fn device_id(&self) -> u32 {
        16
    }

    fn num_queues(&self) -> usize {
        2
    }

    fn device_features(&self) -> u64 {
        VIRTIO_GPU_F_EDID
    }

    fn read_config(&self, offset: u64, size: AccessSize) -> u64 {
        let mut cfg = [0u8; 16];
        cfg[0..4].copy_from_slice(&self.events_read.to_le_bytes());
        cfg[8..12].copy_from_slice(&1u32.to_le_bytes()); // num_scanouts = 1
        let off = offset as usize;
        let n = size.bytes() as usize;
        if off + n > cfg.len() {
            return 0;
        }
        let mut v = 0u64;
        for i in 0..n {
            v |= u64::from(cfg[off + i]) << (8 * i);
        }
        v
    }

    fn write_config(&mut self, offset: u64, size: AccessSize, value: u64) {
        if offset == 4 && size == AccessSize::Word {
            self.events_read &= !(value as u32);
        }
    }

    fn process_queue(
        &mut self,
        queue_idx: usize,
        queue: &mut VirtQueue,
        dma: &DmaRam,
        event_idx: bool,
    ) -> u32 {
        let mut used = 0;
        while let Some(chain) = queue.pop(dma) {
            let Some(req) = chain.read_all(dma) else {
                queue.push_used(dma, chain.head, 0, event_idx);
                used += 1;
                continue;
            };
            if queue_idx == 0 {
                let resp = self.handle_ctrl_cmd(&req, dma);
                let written = chain.write_all(dma, &resp).unwrap_or(0);
                queue.push_used(dma, chain.head, written, event_idx);
            } else {
                self.handle_cursor_cmd(&req);
                let resp = Self::build_hdr(VIRTIO_GPU_RESP_OK_NODATA, 0, 0);
                let written = chain.write_all(dma, &resp).unwrap_or(0);
                queue.push_used(dma, chain.head, written, event_idx);
            }
            used += 1;
        }
        used
    }
}

/// Builds a valid 128-byte EDID v1.4 block for `width x height` @ 60 Hz.
fn build_edid(width: u32, height: u32) -> [u8; 128] {
    let mut edid = [0u8; 128];
    // Fixed EDID header.
    edid[0..8].copy_from_slice(&[0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00]);
    // Manufacturer ID "RHD" (Red Hat / QEMU), product 0x1234.
    edid[8..10].copy_from_slice(&[0x49, 0x04]);
    edid[10..12].copy_from_slice(&0x1234u16.to_le_bytes());
    edid[16] = 1; // Week 1
    edid[17] = 34; // Year 2024
    edid[18] = 1; // EDID version 1.4
    edid[19] = 4;
    edid[20] = 0xa5; // Digital input, 8 bits per color
    edid[21] = 32; // 32 cm horizontal
    edid[22] = 20; // 20 cm vertical
    edid[23] = 120; // Gamma 2.20
    edid[24] = 0x06; // sRGB, preferred timing in descriptor 1

    // Detailed Timing Descriptor 1 at 54..72.
    let w = width.clamp(64, 4095) as u16;
    let h = height.clamp(64, 4095) as u16;
    let hblank: u16 = 160;
    let vblank: u16 = 45;
    let pclk_10khz =
        ((u32::from(w + hblank) * u32::from(h + vblank) * 60) / 10_000).clamp(1, 65535) as u16;
    edid[54..56].copy_from_slice(&pclk_10khz.to_le_bytes());
    edid[56] = (w & 0xff) as u8;
    edid[57] = (hblank & 0xff) as u8;
    edid[58] = (((w >> 8) & 0x0f) << 4) as u8 | ((hblank >> 8) & 0x0f) as u8;
    edid[59] = (h & 0xff) as u8;
    edid[60] = (vblank & 0xff) as u8;
    edid[61] = (((h >> 8) & 0x0f) << 4) as u8 | ((vblank >> 8) & 0x0f) as u8;
    edid[62] = 48; // H sync offset
    edid[63] = 32; // H sync pulse width
    edid[64] = (3 << 4) | 5; // V sync offset = 3, width = 5
    edid[66] = 0x40;
    edid[67] = 0xc8;
    edid[68] = 0x10;
    edid[71] = 0x18; // Digital separate sync

    // Monitor name descriptor at 72..90 ("rivemu-gpu\n  ").
    edid[72..77].copy_from_slice(&[0x00, 0x00, 0x00, 0xfc, 0x00]);
    edid[77..90].copy_from_slice(b"rivemu-gpu\n  ");

    let sum: u8 = edid[0..127].iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
    edid[127] = 0u8.wrapping_sub(sum);
    edid
}

/// Pure-Rust lossless 24-bit RGB PNG encoder using stored (uncompressed) zlib blocks.
fn encode_rgb_png(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    // PNG magic signature.
    out.extend_from_slice(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);

    // IHDR chunk: width (4), height (4), bit_depth=8, color_type=2 (RGB), comp=0, filter=0, interlace=0.
    let mut ihdr = [0u8; 13];
    ihdr[0..4].copy_from_slice(&width.to_be_bytes());
    ihdr[4..8].copy_from_slice(&height.to_be_bytes());
    ihdr[8] = 8; // 8 bits per channel
    ihdr[9] = 2; // Truecolor RGB
    write_png_chunk(&mut out, *b"IHDR", &ihdr);

    // Build raw filtered scanlines: each row has filter byte `0` followed by `width * 3` RGB bytes.
    let row_bytes = 1 + (width as usize) * 3;
    let mut raw = Vec::with_capacity(row_bytes * (height as usize));
    for y in 0..height as usize {
        raw.push(0); // Filter type 0 (None)
        let row_start = y * (width as usize) * 4;
        for x in 0..width as usize {
            let idx = row_start + x * 4;
            raw.push(rgba[idx]);
            raw.push(rgba[idx + 1]);
            raw.push(rgba[idx + 2]);
        }
    }

    // Wrap `raw` in a valid zlib stream (CMF=0x78, FLG=0x01, stored DEFLATE blocks, Adler-32).
    let mut zlib = Vec::with_capacity(raw.len() + (raw.len() / 65535 + 1) * 5 + 6);
    zlib.push(0x78);
    zlib.push(0x01);
    let chunks: Vec<&[u8]> = if raw.is_empty() {
        vec![&[]]
    } else {
        raw.chunks(65535).collect()
    };
    for (i, blk) in chunks.iter().enumerate() {
        let is_last = i + 1 == chunks.len();
        zlib.push(u8::from(is_last));
        let len = blk.len() as u16;
        zlib.extend_from_slice(&len.to_le_bytes());
        zlib.extend_from_slice(&(!len).to_le_bytes());
        zlib.extend_from_slice(blk);
    }
    zlib.extend_from_slice(&adler32(&raw).to_be_bytes());
    write_png_chunk(&mut out, *b"IDAT", &zlib);
    write_png_chunk(&mut out, *b"IEND", &[]);
    out
}

fn write_png_chunk(out: &mut Vec<u8>, tag: [u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let crc_start = out.len();
    out.extend_from_slice(&tag);
    out.extend_from_slice(data);
    let crc = crc32_ieee(&out[crc_start..]);
    out.extend_from_slice(&crc.to_be_bytes());
}

fn adler32(data: &[u8]) -> u32 {
    let mut a: u32 = 1;
    let mut b: u32 = 0;
    for &byte in data {
        a = (a + u32::from(byte)) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}

fn crc32_ieee(data: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            if crc & 1 != 0 {
                crc = (crc >> 1) ^ 0xedb8_8320;
            } else {
                crc >>= 1;
            }
        }
    }
    !crc
}
