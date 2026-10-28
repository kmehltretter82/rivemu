//! Intel/Sharp CFI01 NOR Flash (`cfi-flash` / `cfi.pflash01` at `0x2000_0000`).
//!
//! Uses sparse 256 KiB erase blocks (`None` = erased `0xff`) so 64 MiB of
//! mapped NOR flash costs ~2 KiB when untouched and erases blocks in O(1).

use std::sync::Mutex;

use rivemu_core::AccessSize;
use rivemu_mem::MmioDevice;

/// Standard erase-block size (256 KiB).
pub(crate) const BLOCK_SIZE: usize = 256 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    ReadArray,
    ReadStatus,
    ReadId,
    CfiQuery,
    Program,
    EraseSetup,
    LockSetup,
    WriteBufferCount,
    WriteBufferData { remaining: usize },
}

#[derive(Debug)]
struct State {
    mode: Mode,
    status: u8,
    blocks: Vec<Option<Box<[u8; BLOCK_SIZE]>>>,
}

/// CFI01 NOR flash device.
#[derive(Debug)]
pub struct PflashCfi01 {
    size: u64,
    state: Mutex<State>,
}

impl PflashCfi01 {
    /// Creates an erased (`0xff`) CFI01 flash region of `size` bytes (rounded up to 256 KiB).
    pub fn new(size: u64) -> Self {
        let num_blocks = (size as usize).div_ceil(BLOCK_SIZE).max(1);
        let mut blocks = Vec::with_capacity(num_blocks);
        blocks.resize_with(num_blocks, || None);
        Self {
            size: (num_blocks * BLOCK_SIZE) as u64,
            state: Mutex::new(State {
                mode: Mode::ReadArray,
                status: 0x80,
                blocks,
            }),
        }
    }

    /// Loads initial firmware or variable data at `offset` inside the flash array.
    pub fn load_bytes(&self, offset: u64, data: &[u8]) {
        let mut s = self.state.lock().unwrap();
        for (i, &b) in data.iter().enumerate() {
            let pos = offset as usize + i;
            let blk = pos / BLOCK_SIZE;
            let off = pos % BLOCK_SIZE;
            if blk < s.blocks.len() && b != 0xff {
                let buf = s.blocks[blk].get_or_insert_with(|| Box::new([0xff; BLOCK_SIZE]));
                buf[off] = b;
            }
        }
    }

    fn cfi_byte(&self, index: u64) -> u8 {
        let log2_size = (self.size.trailing_zeros() as u8).max(18);
        let num_blocks = (self.size as usize / BLOCK_SIZE).max(1);
        let n_minus_1 = (num_blocks - 1) as u16;
        let blk_256 = (BLOCK_SIZE / 256) as u16;
        match index {
            0x10 => b'Q',
            0x11 => b'R',
            0x12 => b'Y',
            0x13 => 0x01, // Intel/Sharp extended command set (0x0001)
            0x14 => 0x00,
            0x15 => 0x31, // Primary extended table offset
            0x16 => 0x00,
            0x1b => 0x45, // Vcc min 4.5V
            0x1c => 0x55, // Vcc max 5.5V
            0x1f => 0x04, // Typical single word timeout 2^4 us
            0x20 => 0x04, // Typical buffer write timeout 2^4 us
            0x21 => 0x08, // Typical block erase timeout 2^8 ms
            0x23 => 0x01, // Max word timeout multiplier
            0x24 => 0x01, // Max buffer write timeout multiplier
            0x25 => 0x01, // Max block erase timeout multiplier
            0x27 => log2_size,
            0x28 => 0x02, // x8/x16/x32 interface
            0x29 => 0x00,
            0x2a => 0x08, // Max write buffer 2^8 = 256 bytes
            0x2b => 0x00,
            0x2c => 0x01, // 1 erase block region
            0x2d => (n_minus_1 & 0xff) as u8,
            0x2e => (n_minus_1 >> 8) as u8,
            0x2f => (blk_256 & 0xff) as u8,
            0x30 => (blk_256 >> 8) as u8,
            0x31 => b'P',
            0x32 => b'R',
            0x33 => b'I',
            0x34 => b'1',
            0x35 => b'0',
            _ => 0,
        }
    }

    fn read_array_byte(s: &State, pos: usize) -> u8 {
        let blk = pos / BLOCK_SIZE;
        let off = pos % BLOCK_SIZE;
        s.blocks
            .get(blk)
            .and_then(|b| b.as_ref().map(|arr| arr[off]))
            .unwrap_or(0xff)
    }

    fn write_array_byte(s: &mut State, pos: usize, val: u8) {
        let blk = pos / BLOCK_SIZE;
        let off = pos % BLOCK_SIZE;
        if blk < s.blocks.len() {
            let buf = s.blocks[blk].get_or_insert_with(|| Box::new([0xff; BLOCK_SIZE]));
            buf[off] = val;
        }
    }
}

impl MmioDevice for PflashCfi01 {
    fn read(&self, offset: u64, size: AccessSize) -> u64 {
        let s = self.state.lock().unwrap();
        let n = size.bytes() as usize;
        match s.mode {
            Mode::ReadArray => {
                let mut v = 0u64;
                for i in 0..n {
                    let b = Self::read_array_byte(&s, offset as usize + i);
                    v |= u64::from(b) << (8 * i);
                }
                v
            }
            Mode::ReadStatus
            | Mode::Program
            | Mode::EraseSetup
            | Mode::LockSetup
            | Mode::WriteBufferCount
            | Mode::WriteBufferData { .. } => u64::from(s.status),
            Mode::ReadId => match (offset >> 2) & 3 {
                0 => 0x0089, // Intel manufacturer ID
                1 => 0x0018, // 128 Mbit / 256 Mbit StrataFlash ID
                _ => 0,
            },
            Mode::CfiQuery => {
                // Support 1-byte, 2-byte, or 4-byte interleave queries (`offset / n`).
                let idx = offset / (n as u64).max(1);
                u64::from(self.cfi_byte(idx))
            }
        }
    }

    fn write(&self, offset: u64, size: AccessSize, value: u64) {
        let mut s = self.state.lock().unwrap();
        let n = size.bytes() as usize;
        match s.mode {
            Mode::Program => {
                for i in 0..n {
                    Self::write_array_byte(&mut s, offset as usize + i, (value >> (8 * i)) as u8);
                }
                s.status = 0x80;
                s.mode = Mode::ReadStatus;
                return;
            }
            Mode::EraseSetup => {
                if (value & 0xff) as u8 == 0xd0 {
                    let blk = (offset as usize) / BLOCK_SIZE;
                    if blk < s.blocks.len() {
                        s.blocks[blk] = None;
                    }
                    s.status = 0x80;
                    s.mode = Mode::ReadStatus;
                } else {
                    s.mode = Mode::ReadArray;
                }
                return;
            }
            Mode::LockSetup => {
                s.status = 0x80;
                s.mode = Mode::ReadStatus;
                return;
            }
            Mode::WriteBufferCount => {
                let words = (value & 0xff) as usize;
                s.mode = Mode::WriteBufferData {
                    remaining: words + 1,
                };
                return;
            }
            Mode::WriteBufferData { remaining } => {
                for i in 0..n {
                    Self::write_array_byte(&mut s, offset as usize + i, (value >> (8 * i)) as u8);
                }
                if remaining <= 1 {
                    s.status = 0x80;
                    s.mode = Mode::ReadStatus;
                } else {
                    s.mode = Mode::WriteBufferData {
                        remaining: remaining - 1,
                    };
                }
                return;
            }
            _ => {}
        }

        // Standard CFI01 command byte dispatch.
        match (value & 0xff) as u8 {
            0xff | 0xf0 => s.mode = Mode::ReadArray,
            0x70 => s.mode = Mode::ReadStatus,
            0x50 => {
                s.status = 0x80;
                s.mode = Mode::ReadStatus;
            }
            0x90 => s.mode = Mode::ReadId,
            0x98 => s.mode = Mode::CfiQuery,
            0x40 | 0x10 => s.mode = Mode::Program,
            0x20 => s.mode = Mode::EraseSetup,
            0x60 => s.mode = Mode::LockSetup,
            0xe8 => {
                s.status = 0x80;
                s.mode = Mode::WriteBufferCount;
            }
            0xd0 => {
                s.status = 0x80;
                s.mode = Mode::ReadStatus;
            }
            _ => s.mode = Mode::ReadArray,
        }
    }
}
