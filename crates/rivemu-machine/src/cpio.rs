//! In-memory SVR4 `newc` (`070701`) cpio initramfs builder for `-run-init`.
//!
//! Packages a compiled `riscv64gc-unknown-linux-musl` binary as `/init` inside
//! an ephemeral initramfs archive (along with `/dev`, `/proc`, `/sys`, `/tmp`,
//! and `/mnt` directories) so `rivemu` can boot Linux and run any Rust test
//! binary directly as PID 1 without external `cpio` tooling.

/// Builds a valid Linux `newc` (`070701`) cpio archive containing `/init` with
/// the given executable bytes and standard mount-point directories.
pub fn build_initramfs(init_elf: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(init_elf.len() + 2048);
    let mut ino = 1u32;
    for dir in ["dev", "proc", "sys", "tmp", "mnt"] {
        append_entry(&mut out, ino, 0o040_755, 2, dir, &[]);
        ino += 1;
    }
    append_entry(&mut out, ino, 0o100_755, 1, "init", init_elf);
    append_entry(&mut out, 0, 0, 1, "TRAILER!!!", &[]);
    while out.len() & 511 != 0 {
        out.push(0);
    }
    out
}

fn append_entry(out: &mut Vec<u8>, ino: u32, mode: u32, nlink: u32, name: &str, data: &[u8]) {
    let namesize = name.len() + 1;
    let hdr = format!(
        "070701\
         {ino:08X}\
         {mode:08X}\
         00000000\
         00000000\
         {nlink:08X}\
         00000000\
         {:08X}\
         00000000\
         00000000\
         00000000\
         00000000\
         {namesize:08X}\
         00000000",
        data.len()
    );
    debug_assert_eq!(hdr.len(), 110);
    out.extend_from_slice(hdr.as_bytes());
    out.extend_from_slice(name.as_bytes());
    out.push(0);
    while out.len() & 3 != 0 {
        out.push(0);
    }
    out.extend_from_slice(data);
    while out.len() & 3 != 0 {
        out.push(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_valid_newc_cpio_archive() {
        let bin = b"\x7fELF_FAKE_INIT_BINARY";
        let cpio = build_initramfs(bin);
        assert_eq!(&cpio[..6], b"070701");
        assert_eq!(cpio.len() % 512, 0);
        assert!(cpio.windows(4).any(|w| w == b"init"));
        assert!(cpio.windows(bin.len()).any(|w| w == bin));
        assert!(cpio.windows(10).any(|w| w == b"TRAILER!!!"));
    }
}
