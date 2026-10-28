//! QEMU Firmware Configuration Device (`qemu,fw-cfg-mmio` at `0x1010_0000`).
//!
//! Used by EDK2 UEFI, U-Boot, and the Linux `qemu_fw_cfg` driver to read
//! host-provided configuration items and named blobs.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use rivemu_core::AccessSize;
use rivemu_mem::{MmioDevice, Ram};

/// Standard `fw_cfg` selector keys.
pub(crate) mod key {
    /// Signature (`"QEMU"`).
    pub(crate) const SIGNATURE: u16 = 0x0000;
    /// Feature bitmap (`1` = traditional, `2` = DMA).
    pub(crate) const ID: u16 = 0x0001;
    /// Guest RAM size in bytes (`u64` LE).
    pub(crate) const RAM_SIZE: u16 = 0x0003;
    /// Number of online CPUs (`u16` LE).
    pub(crate) const NB_CPUS: u16 = 0x0005;
    /// Maximum number of CPUs (`u16` LE).
    pub(crate) const MAX_CPUS: u16 = 0x000f;
    /// File directory (`u32` BE count followed by 64-byte `FWCfgFile` entries).
    pub(crate) const FILE_DIR: u16 = 0x0019;
    /// First dynamic file slot.
    pub(crate) const FILE_FIRST: u16 = 0x0020;
}

const DMA_SIGNATURE: u64 = 0x5145_4d55_2043_4647; // "QEMU CFG"

#[derive(Debug)]
struct State {
    selector: u16,
    offset: usize,
    dma_addr: u64,
    items: HashMap<u16, Vec<u8>>,
    files: Vec<(String, u16, usize)>,
}

/// QEMU `fw_cfg` MMIO device (`0x18` bytes).
#[derive(Debug)]
pub struct FwCfg {
    state: Mutex<State>,
    dram_base: u64,
    ram: Option<Arc<Ram>>,
}

impl FwCfg {
    /// Creates a `fw_cfg` device populated with standard machine items.
    pub fn new(ram_size: u64, smp: u16, dram_base: u64, ram: Option<Arc<Ram>>) -> Self {
        let mut items = HashMap::new();
        items.insert(key::SIGNATURE, b"QEMU".to_vec());
        items.insert(key::ID, 3u32.to_le_bytes().to_vec());
        items.insert(key::RAM_SIZE, ram_size.to_le_bytes().to_vec());
        items.insert(key::NB_CPUS, smp.to_le_bytes().to_vec());
        items.insert(key::MAX_CPUS, smp.to_le_bytes().to_vec());
        items.insert(key::FILE_DIR, 0u32.to_be_bytes().to_vec());

        Self {
            state: Mutex::new(State {
                selector: 0,
                offset: 0,
                dma_addr: 0,
                items,
                files: Vec::new(),
            }),
            dram_base,
            ram,
        }
    }

    /// Adds a named file to the `fw_cfg` file directory (`FW_CFG_FILE_DIR`).
    pub fn add_file(&self, name: impl Into<String>, data: Vec<u8>) {
        let mut s = self.state.lock().unwrap();
        let slot = key::FILE_FIRST + s.files.len() as u16;
        let len = data.len();
        s.items.insert(slot, data);
        s.files.push((name.into(), slot, len));

        // Rebuild FW_CFG_FILE_DIR (big-endian count + 64-byte entries).
        let mut dir = Vec::with_capacity(4 + s.files.len() * 64);
        dir.extend_from_slice(&(s.files.len() as u32).to_be_bytes());
        for (fname, fslot, flen) in &s.files {
            dir.extend_from_slice(&(*flen as u32).to_be_bytes());
            dir.extend_from_slice(&fslot.to_be_bytes());
            dir.extend_from_slice(&0u16.to_be_bytes());
            let mut name_buf = [0u8; 56];
            let bytes = fname.as_bytes();
            let n = bytes.len().min(55);
            name_buf[..n].copy_from_slice(&bytes[..n]);
            dir.extend_from_slice(&name_buf);
        }
        s.items.insert(key::FILE_DIR, dir);
    }

    fn handle_dma(&self, s: &mut State, desc_pa: u64) {
        let Some(ram) = &self.ram else {
            return;
        };
        let Some(desc_off) = desc_pa.checked_sub(self.dram_base) else {
            return;
        };
        let Some(ctrl_len) = ram.read(desc_off, AccessSize::Double) else {
            return;
        };
        let Some(addr_raw) = ram.read(desc_off + 8, AccessSize::Double) else {
            return;
        };
        let control = u32::from_be((ctrl_len as u32).to_be());
        // Descriptor fields in guest RAM are big-endian.
        let desc_bytes = [
            ctrl_len.to_le_bytes()[0],
            ctrl_len.to_le_bytes()[1],
            ctrl_len.to_le_bytes()[2],
            ctrl_len.to_le_bytes()[3],
            ctrl_len.to_le_bytes()[4],
            ctrl_len.to_le_bytes()[5],
            ctrl_len.to_le_bytes()[6],
            ctrl_len.to_le_bytes()[7],
        ];
        let _ = control;
        let ctrl = u32::from_be_bytes([desc_bytes[0], desc_bytes[1], desc_bytes[2], desc_bytes[3]]);
        let length =
            u32::from_be_bytes([desc_bytes[4], desc_bytes[5], desc_bytes[6], desc_bytes[7]])
                as usize;
        let target_pa = u64::from_be_bytes(addr_raw.to_le_bytes());

        // Bit 3: SELECT (`ctrl >> 16` is the selector key).
        if ctrl & (1 << 3) != 0 {
            s.selector = (ctrl >> 16) as u16;
            s.offset = 0;
        }
        // Bit 1: READ (`length` bytes to `target_pa`).
        if ctrl & (1 << 1) != 0 {
            if let Some(dst_off) = target_pa.checked_sub(self.dram_base) {
                for i in 0..length {
                    let b = s
                        .items
                        .get(&s.selector)
                        .and_then(|v| v.get(s.offset).copied())
                        .unwrap_or(0);
                    s.offset = s.offset.saturating_add(1);
                    let _ = ram.write(dst_off + i as u64, AccessSize::Byte, u64::from(b));
                }
            }
        } else if ctrl & (1 << 2) != 0 {
            // Bit 2: SKIP.
            s.offset = s.offset.saturating_add(length);
        }
        // Clear control word to 0 (transfer complete, no error).
        let _ = ram.write(desc_off, AccessSize::Word, 0);
    }
}

impl MmioDevice for FwCfg {
    fn read(&self, offset: u64, size: AccessSize) -> u64 {
        let mut s = self.state.lock().unwrap();
        match offset {
            0x00..=0x07 => {
                let n = size.bytes() as usize;
                let mut val = 0u64;
                for i in 0..n {
                    let b = s
                        .items
                        .get(&s.selector)
                        .and_then(|v| v.get(s.offset).copied())
                        .unwrap_or(0);
                    s.offset = s.offset.saturating_add(1);
                    val |= u64::from(b) << (8 * i);
                }
                val
            }
            0x08..=0x09 => u64::from(s.selector),
            0x10..=0x17 => {
                // Big-endian "QEMU CFG" signature across offsets 0x10..0x17.
                let be_bytes = DMA_SIGNATURE.to_be_bytes();
                let start = (offset - 0x10) as usize;
                let n = (size.bytes() as usize).min(8 - start);
                let mut out = 0u64;
                for i in 0..n {
                    out |= u64::from(be_bytes[start + i]) << (8 * i);
                }
                out
            }
            _ => 0,
        }
    }

    fn write(&self, offset: u64, size: AccessSize, value: u64) {
        let mut s = self.state.lock().unwrap();
        match offset {
            0x08..=0x09 => {
                let raw = value as u16;
                // Linux `qemu_fw_cfg` writes the 16-bit selector in big-endian
                // (`iowrite16be`), whereas bare-metal tests may write native LE.
                let swapped = raw.swap_bytes();
                s.selector = if s.items.contains_key(&swapped) && !s.items.contains_key(&raw) {
                    swapped
                } else {
                    raw
                };
                s.offset = 0;
            }
            0x10..=0x17 => {
                if size == AccessSize::Double && offset == 0x10 {
                    let pa = u64::from_be_bytes(value.to_le_bytes());
                    self.handle_dma(&mut s, pa);
                } else if size == AccessSize::Word && offset == 0x10 {
                    let hi = u32::from_be_bytes((value as u32).to_le_bytes());
                    s.dma_addr = u64::from(hi) << 32;
                } else if size == AccessSize::Word && offset == 0x14 {
                    let lo = u32::from_be_bytes((value as u32).to_le_bytes());
                    let pa = s.dma_addr | u64::from(lo);
                    s.dma_addr = 0;
                    self.handle_dma(&mut s, pa);
                }
            }
            _ => {}
        }
    }
}
