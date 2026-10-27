//! Minimal flattened device tree (DTB, version 17) builder.
//!
//! ```
//! let mut fdt = rivemu_fdt::FdtBuilder::new();
//! fdt.begin_node("");
//! fdt.prop_u32("#address-cells", 2);
//! fdt.begin_node("chosen");
//! fdt.prop_str("bootargs", "console=ttyS0");
//! fdt.end_node();
//! fdt.end_node();
//! let blob = fdt.finish(0);
//! assert_eq!(&blob[..4], &[0xd0, 0x0d, 0xfe, 0xed]);
//! ```

#![forbid(unsafe_code)]

use std::collections::HashMap;

const MAGIC: u32 = 0xd00d_feed;
const BEGIN_NODE: u32 = 1;
const END_NODE: u32 = 2;
const PROP: u32 = 3;
const END: u32 = 9;
const HEADER_SIZE: usize = 40;

/// Incrementally builds a device tree blob.
#[derive(Debug, Default)]
pub struct FdtBuilder {
    structs: Vec<u8>,
    strings: Vec<u8>,
    string_offsets: HashMap<String, u32>,
    reservations: Vec<(u64, u64)>,
    depth: usize,
    next_phandle: u32,
}

impl FdtBuilder {
    /// Creates an empty builder.
    pub fn new() -> Self {
        Self {
            next_phandle: 1,
            ..Self::default()
        }
    }

    /// Allocates a fresh phandle value.
    pub fn alloc_phandle(&mut self) -> u32 {
        let p = self.next_phandle;
        self.next_phandle += 1;
        p
    }

    /// Adds a memory reservation entry.
    pub fn reserve_memory(&mut self, addr: u64, size: u64) {
        self.reservations.push((addr, size));
    }

    fn token(&mut self, t: u32) {
        self.structs.extend_from_slice(&t.to_be_bytes());
    }

    fn pad(&mut self) {
        while self.structs.len() % 4 != 0 {
            self.structs.push(0);
        }
    }

    /// Opens a node (use `""` for the root node).
    pub fn begin_node(&mut self, name: &str) {
        self.token(BEGIN_NODE);
        self.structs.extend_from_slice(name.as_bytes());
        self.structs.push(0);
        self.pad();
        self.depth += 1;
    }

    /// Closes the innermost open node.
    pub fn end_node(&mut self) {
        assert!(self.depth > 0, "end_node without begin_node");
        self.token(END_NODE);
        self.depth -= 1;
    }

    fn string_offset(&mut self, name: &str) -> u32 {
        if let Some(&off) = self.string_offsets.get(name) {
            return off;
        }
        let off = self.strings.len() as u32;
        self.strings.extend_from_slice(name.as_bytes());
        self.strings.push(0);
        self.string_offsets.insert(name.to_owned(), off);
        off
    }

    /// Adds a property with a raw value.
    pub fn prop_bytes(&mut self, name: &str, value: &[u8]) {
        assert!(self.depth > 0, "property outside of a node");
        let off = self.string_offset(name);
        self.token(PROP);
        self.token(value.len() as u32);
        self.token(off);
        self.structs.extend_from_slice(value);
        self.pad();
    }

    /// Adds an empty (boolean) property.
    pub fn prop_null(&mut self, name: &str) {
        self.prop_bytes(name, &[]);
    }

    /// Adds a single 32-bit cell.
    pub fn prop_u32(&mut self, name: &str, v: u32) {
        self.prop_bytes(name, &v.to_be_bytes());
    }

    /// Adds a 64-bit value (two cells).
    pub fn prop_u64(&mut self, name: &str, v: u64) {
        self.prop_bytes(name, &v.to_be_bytes());
    }

    /// Adds a list of 32-bit cells.
    pub fn prop_cells(&mut self, name: &str, cells: &[u32]) {
        let bytes: Vec<u8> = cells.iter().flat_map(|c| c.to_be_bytes()).collect();
        self.prop_bytes(name, &bytes);
    }

    /// Adds a list of 64-bit values (each as two cells), e.g. `reg` with
    /// `#address-cells = #size-cells = 2`.
    pub fn prop_u64s(&mut self, name: &str, values: &[u64]) {
        let bytes: Vec<u8> = values.iter().flat_map(|v| v.to_be_bytes()).collect();
        self.prop_bytes(name, &bytes);
    }

    /// Adds a string property.
    pub fn prop_str(&mut self, name: &str, s: &str) {
        self.prop_strs(name, &[s]);
    }

    /// Adds a string-list property.
    pub fn prop_strs(&mut self, name: &str, list: &[&str]) {
        let mut bytes = Vec::new();
        for s in list {
            bytes.extend_from_slice(s.as_bytes());
            bytes.push(0);
        }
        self.prop_bytes(name, &bytes);
    }

    /// Finishes the blob. All nodes must be closed.
    pub fn finish(mut self, boot_cpuid: u32) -> Vec<u8> {
        assert_eq!(self.depth, 0, "unclosed device tree nodes");
        self.token(END);

        let rsvmap_off = HEADER_SIZE;
        let rsvmap_size = (self.reservations.len() + 1) * 16;
        let struct_off = rsvmap_off + rsvmap_size;
        let strings_off = struct_off + self.structs.len();
        let total = strings_off + self.strings.len();

        let mut out = Vec::with_capacity(total);
        for v in [
            MAGIC,
            total as u32,
            struct_off as u32,
            strings_off as u32,
            rsvmap_off as u32,
            17,
            16,
            boot_cpuid,
            self.strings.len() as u32,
            self.structs.len() as u32,
        ] {
            out.extend_from_slice(&v.to_be_bytes());
        }
        for (addr, size) in self.reservations.iter().chain(std::iter::once(&(0, 0))) {
            out.extend_from_slice(&addr.to_be_bytes());
            out.extend_from_slice(&size.to_be_bytes());
        }
        out.extend_from_slice(&self.structs);
        out.extend_from_slice(&self.strings);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn be32(b: &[u8], off: usize) -> u32 {
        u32::from_be_bytes(b[off..off + 4].try_into().unwrap())
    }

    #[test]
    fn layout() {
        let mut f = FdtBuilder::new();
        f.reserve_memory(0x8000_0000, 0x1000);
        f.begin_node("");
        f.prop_u32("#size-cells", 2);
        f.begin_node("memory@80000000");
        f.prop_str("device_type", "memory");
        f.prop_u64s("reg", &[0x8000_0000, 0x800_0000]);
        f.end_node();
        f.begin_node("cpus");
        f.prop_u32("#size-cells", 0); // name deduplicated
        f.end_node();
        f.end_node();
        let b = f.finish(0);

        assert_eq!(be32(&b, 0), MAGIC);
        assert_eq!(be32(&b, 4) as usize, b.len());
        assert_eq!(be32(&b, 20), 17);
        let rsv = be32(&b, 16) as usize;
        assert_eq!(&b[rsv..rsv + 8], &0x8000_0000u64.to_be_bytes());
        let strings = be32(&b, 12) as usize;
        let strtab = &b[strings..];
        assert_eq!(strtab, b"#size-cells\0device_type\0reg\0");
        let structs = be32(&b, 8) as usize;
        assert_eq!(be32(&b, structs), BEGIN_NODE);
        assert_eq!(be32(&b, structs + be32(&b, 36) as usize - 4), END);
    }

    #[test]
    fn phandles_are_unique() {
        let mut f = FdtBuilder::new();
        let a = f.alloc_phandle();
        let b = f.alloc_phandle();
        assert_ne!(a, b);
        assert_ne!(a, 0);
    }

    #[test]
    #[should_panic(expected = "unclosed")]
    fn unclosed_node_panics() {
        let mut f = FdtBuilder::new();
        f.begin_node("");
        let _ = f.finish(0);
    }
}
