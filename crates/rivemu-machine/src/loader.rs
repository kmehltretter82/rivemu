//! Guest image loaders: ELF64 (RISC-V, little-endian) and raw binaries.

use std::collections::HashMap;

use rivemu_core::GuestAddr;
use rivemu_mem::{AddressSpace, BusError};
use thiserror::Error;

/// `e_machine` value for RISC-V.
pub const EM_RISCV: u16 = 243;

const PT_LOAD: u32 = 1;
const SHT_SYMTAB: u32 = 2;
const ET_EXEC: u16 = 2;
const ET_DYN: u16 = 3;
const EHDR_SIZE: usize = 64;
const PHDR_SIZE: u64 = 56;
const SHDR_SIZE: u64 = 64;
const SYM_SIZE: u64 = 24;

/// Image loading error.
#[derive(Debug, Error)]
pub enum LoadError {
    /// File lacks the ELF magic.
    #[error("not an ELF file")]
    NotElf,
    /// Valid ELF, but not something we can run.
    #[error("unsupported ELF: {0}")]
    Unsupported(&'static str),
    /// Truncated or inconsistent ELF.
    #[error("malformed ELF: {0}")]
    Malformed(&'static str),
    /// Image does not fit into guest memory.
    #[error("image does not fit into guest memory: {0}")]
    Bus(#[from] BusError),
}

/// Result of loading an ELF image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElfImage {
    /// Entry point (`e_entry`).
    pub entry: GuestAddr,
    /// Lowest loaded physical address.
    pub low: GuestAddr,
    /// One past the highest loaded physical address.
    pub high: GuestAddr,
    /// Symbol table (name → value), empty if the image is stripped.
    pub symbols: HashMap<String, u64>,
}

/// Returns `true` if `data` starts with the ELF magic.
pub fn is_elf(data: &[u8]) -> bool {
    data.starts_with(b"\x7fELF")
}

/// Bounds-checked little-endian reader over the file image.
struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn bytes(&self, off: u64, len: u64) -> Result<&'a [u8], LoadError> {
        let start = usize::try_from(off).map_err(|_| LoadError::Malformed("offset too large"))?;
        let len = usize::try_from(len).map_err(|_| LoadError::Malformed("size too large"))?;
        start
            .checked_add(len)
            .and_then(|end| self.0.get(start..end))
            .ok_or(LoadError::Malformed("truncated file"))
    }

    fn u8(&self, off: u64) -> Result<u8, LoadError> {
        Ok(self.bytes(off, 1)?[0])
    }

    fn u16(&self, off: u64) -> Result<u16, LoadError> {
        Ok(u16::from_le_bytes(self.bytes(off, 2)?.try_into().unwrap()))
    }

    fn u32(&self, off: u64) -> Result<u32, LoadError> {
        Ok(u32::from_le_bytes(self.bytes(off, 4)?.try_into().unwrap()))
    }

    fn u64(&self, off: u64) -> Result<u64, LoadError> {
        Ok(u64::from_le_bytes(self.bytes(off, 8)?.try_into().unwrap()))
    }
}

/// Loads the `PT_LOAD` segments of a RISC-V ELF64 image at their physical
/// addresses (`p_paddr`), zero-filling `.bss`.
pub fn load_elf(bus: &AddressSpace, data: &[u8]) -> Result<ElfImage, LoadError> {
    if !is_elf(data) {
        return Err(LoadError::NotElf);
    }
    if data.len() < EHDR_SIZE {
        return Err(LoadError::Malformed("truncated ELF header"));
    }
    let r = Reader(data);
    if r.u8(4)? != 2 {
        return Err(LoadError::Unsupported(
            "only 64-bit ELF files are supported",
        ));
    }
    if r.u8(5)? != 1 {
        return Err(LoadError::Unsupported(
            "only little-endian ELF files are supported",
        ));
    }
    if r.u16(18)? != EM_RISCV {
        return Err(LoadError::Unsupported("not a RISC-V executable"));
    }
    if !matches!(r.u16(16)?, ET_EXEC | ET_DYN) {
        return Err(LoadError::Unsupported("not an executable"));
    }

    let entry = r.u64(24)?;
    let phoff = r.u64(32)?;
    let phentsize = u64::from(r.u16(54)?);
    let phnum = u64::from(r.u16(56)?);
    if phnum > 0 && phentsize < PHDR_SIZE {
        return Err(LoadError::Malformed("program header entry too small"));
    }

    let mut low = u64::MAX;
    let mut high = 0u64;
    for i in 0..phnum {
        let ph = phoff
            .checked_add(i * phentsize)
            .ok_or(LoadError::Malformed("program header offset overflow"))?;
        if r.u32(ph)? != PT_LOAD {
            continue;
        }
        let offset = r.u64(ph + 8)?;
        let paddr = r.u64(ph + 24)?;
        let filesz = r.u64(ph + 32)?;
        let memsz = r.u64(ph + 40)?;
        if memsz == 0 {
            continue;
        }
        if filesz > memsz {
            return Err(LoadError::Malformed(
                "segment file size exceeds memory size",
            ));
        }
        let end = paddr
            .checked_add(memsz)
            .ok_or(LoadError::Malformed("segment overflows address space"))?;
        bus.load_bytes(paddr, r.bytes(offset, filesz)?)?;
        if memsz > filesz {
            bus.fill_bytes(paddr + filesz, memsz - filesz, 0)?;
        }
        low = low.min(paddr);
        high = high.max(end);
    }
    if low > high {
        return Err(LoadError::Malformed("no loadable segments"));
    }

    // A broken symbol table should not prevent booting: ignore errors.
    let symbols = read_symbols(&r).unwrap_or_default();
    Ok(ElfImage {
        entry,
        low,
        high,
        symbols,
    })
}

fn read_symbols(r: &Reader<'_>) -> Result<HashMap<String, u64>, LoadError> {
    let mut symbols = HashMap::new();
    let shoff = r.u64(40)?;
    let shentsize = u64::from(r.u16(58)?);
    let shnum = u64::from(r.u16(60)?);
    if shoff == 0 || shnum == 0 || shentsize < SHDR_SIZE {
        return Ok(symbols);
    }
    let section = |i: u64| shoff + i * shentsize;
    for i in 0..shnum {
        let sh = section(i);
        if r.u32(sh + 4)? != SHT_SYMTAB {
            continue;
        }
        let sym_off = r.u64(sh + 24)?;
        let sym_size = r.u64(sh + 32)?;
        let link = u64::from(r.u32(sh + 40)?);
        if link >= shnum {
            return Err(LoadError::Malformed("bad symbol string table index"));
        }
        let strtab = r.bytes(r.u64(section(link) + 24)?, r.u64(section(link) + 32)?)?;
        for s in 0..sym_size / SYM_SIZE {
            let sym = sym_off + s * SYM_SIZE;
            let name_off = r.u32(sym)? as usize;
            let Some(tail) = strtab.get(name_off..) else {
                continue;
            };
            let name = tail.split(|&b| b == 0).next().unwrap_or_default();
            if name.is_empty() {
                continue;
            }
            if let Ok(name) = std::str::from_utf8(name) {
                symbols.insert(name.to_owned(), r.u64(sym + 8)?);
            }
        }
    }
    Ok(symbols)
}

/// Loads a raw binary at `addr`. Returns `(start, end)`.
pub fn load_raw(
    bus: &AddressSpace,
    addr: GuestAddr,
    data: &[u8],
) -> Result<(GuestAddr, GuestAddr), LoadError> {
    let end = addr
        .checked_add(data.len() as u64)
        .ok_or(BusError::Unmapped { addr, size: 1 })?;
    bus.load_bytes(addr, data)?;
    Ok((addr, end))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use rivemu_core::AccessSize;
    use rivemu_mem::Ram;
    use std::sync::Arc;

    /// Builds a minimal RISC-V ELF64 with one PT_LOAD segment and a symbol table.
    pub(crate) fn build_elf(
        paddr: u64,
        entry: u64,
        code: &[u8],
        bss: u64,
        syms: &[(&str, u64)],
    ) -> Vec<u8> {
        let mut f = vec![0u8; 64];
        f[..4].copy_from_slice(b"\x7fELF");
        f[4] = 2; // ELFCLASS64
        f[5] = 1; // little-endian
        f[6] = 1; // EV_CURRENT
        f[16..18].copy_from_slice(&ET_EXEC.to_le_bytes());
        f[18..20].copy_from_slice(&EM_RISCV.to_le_bytes());
        f[24..32].copy_from_slice(&entry.to_le_bytes());
        f[32..40].copy_from_slice(&64u64.to_le_bytes()); // e_phoff
        f[52..54].copy_from_slice(&64u16.to_le_bytes()); // e_ehsize
        f[54..56].copy_from_slice(&56u16.to_le_bytes()); // e_phentsize
        f[56..58].copy_from_slice(&1u16.to_le_bytes()); // e_phnum

        // Program header.
        let code_off = 64 + 56;
        let mut ph = vec![0u8; 56];
        ph[0..4].copy_from_slice(&PT_LOAD.to_le_bytes());
        ph[8..16].copy_from_slice(&(code_off as u64).to_le_bytes());
        ph[16..24].copy_from_slice(&paddr.to_le_bytes());
        ph[24..32].copy_from_slice(&paddr.to_le_bytes());
        ph[32..40].copy_from_slice(&(code.len() as u64).to_le_bytes());
        ph[40..48].copy_from_slice(&(code.len() as u64 + bss).to_le_bytes());
        f.extend_from_slice(&ph);
        f.extend_from_slice(code);

        // String table + symbol table.
        let mut strtab = vec![0u8];
        let mut symtab = vec![0u8; 24]; // null symbol
        for (name, value) in syms {
            let mut s = vec![0u8; 24];
            s[0..4].copy_from_slice(&(strtab.len() as u32).to_le_bytes());
            s[8..16].copy_from_slice(&value.to_le_bytes());
            symtab.extend_from_slice(&s);
            strtab.extend_from_slice(name.as_bytes());
            strtab.push(0);
        }
        let strtab_off = f.len() as u64;
        f.extend_from_slice(&strtab);
        let symtab_off = f.len() as u64;
        f.extend_from_slice(&symtab);

        // Section headers: [0] null, [1] symtab, [2] strtab.
        let shoff = f.len() as u64;
        f.extend_from_slice(&[0u8; 64]);
        let mut sh = vec![0u8; 64];
        sh[4..8].copy_from_slice(&SHT_SYMTAB.to_le_bytes());
        sh[24..32].copy_from_slice(&symtab_off.to_le_bytes());
        sh[32..40].copy_from_slice(&(symtab.len() as u64).to_le_bytes());
        sh[40..44].copy_from_slice(&2u32.to_le_bytes());
        f.extend_from_slice(&sh);
        let mut sh = vec![0u8; 64];
        sh[4..8].copy_from_slice(&3u32.to_le_bytes()); // SHT_STRTAB
        sh[24..32].copy_from_slice(&strtab_off.to_le_bytes());
        sh[32..40].copy_from_slice(&(strtab.len() as u64).to_le_bytes());
        f.extend_from_slice(&sh);

        f[40..48].copy_from_slice(&shoff.to_le_bytes());
        f[58..60].copy_from_slice(&64u16.to_le_bytes());
        f[60..62].copy_from_slice(&3u16.to_le_bytes());
        f
    }

    fn bus() -> AddressSpace {
        let mut b = AddressSpace::new();
        b.add_ram("dram", 0x8000_0000, Arc::new(Ram::new(0x10000).unwrap()))
            .unwrap();
        b
    }

    #[test]
    fn loads_segments_bss_and_symbols() {
        let b = bus();
        b.fill_bytes(0x8000_0000, 0x100, 0xff).unwrap();
        let elf = build_elf(
            0x8000_0000,
            0x8000_0004,
            &[0x13, 0, 0, 0, 0x6f, 0, 0, 0],
            16,
            &[("tohost", 0x8000_1000), ("_start", 0x8000_0000)],
        );
        let img = load_elf(&b, &elf).unwrap();
        assert_eq!(img.entry, 0x8000_0004);
        assert_eq!((img.low, img.high), (0x8000_0000, 0x8000_0018));
        assert_eq!(img.symbols["tohost"], 0x8000_1000);
        assert_eq!(img.symbols["_start"], 0x8000_0000);
        assert_eq!(b.read(0x8000_0004, AccessSize::Word).unwrap(), 0x6f);
        // .bss zeroed, byte after it untouched.
        assert_eq!(b.read(0x8000_0008, AccessSize::Double).unwrap(), 0);
        assert_eq!(b.read(0x8000_0010, AccessSize::Double).unwrap(), 0);
        assert_eq!(b.read(0x8000_0018, AccessSize::Byte).unwrap(), 0xff);
    }

    #[test]
    fn rejects_bad_files() {
        let b = bus();
        assert!(matches!(load_elf(&b, b"hello"), Err(LoadError::NotElf)));
        assert!(matches!(
            load_elf(&b, b"\x7fELF\x02\x01"),
            Err(LoadError::Malformed(_))
        ));

        let good = build_elf(0x8000_0000, 0x8000_0000, &[0; 4], 0, &[]);
        let mut e = good.clone();
        e[4] = 1;
        assert!(matches!(load_elf(&b, &e), Err(LoadError::Unsupported(_))));
        let mut e = good.clone();
        e[18] = 62; // x86-64
        assert!(matches!(load_elf(&b, &e), Err(LoadError::Unsupported(_))));
        // Truncated inside the segment data.
        let e = &good[..64 + 56 + 2];
        assert!(matches!(load_elf(&b, e), Err(LoadError::Malformed(_))));
        // Truncated symbol table only: still loads, symbols are dropped.
        let with_syms = build_elf(0x8000_0000, 0x8000_0000, &[0; 4], 0, &[("x", 1)]);
        let img = load_elf(&b, &with_syms[..with_syms.len() - 40]).unwrap();
        assert!(img.symbols.is_empty());
    }

    #[test]
    fn segment_outside_ram_fails() {
        let b = bus();
        let elf = build_elf(0x4000_0000, 0x4000_0000, &[0; 4], 0, &[]);
        assert!(matches!(load_elf(&b, &elf), Err(LoadError::Bus(_))));
    }

    #[test]
    fn raw_load() {
        let b = bus();
        assert_eq!(
            load_raw(&b, 0x8000_0100, &[1, 2, 3, 4]).unwrap(),
            (0x8000_0100, 0x8000_0104)
        );
        assert_eq!(b.read(0x8000_0100, AccessSize::Word).unwrap(), 0x0403_0201);
        assert!(load_raw(&b, 0x8000_fffe, &[0; 4]).is_err());
    }
}
