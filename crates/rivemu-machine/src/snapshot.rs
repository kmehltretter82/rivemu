//! Fast sparse VM snapshot save and restore (`savevm` / `loadvm`).
//!
//! Serializes all hart registers/CSRs, interrupt controller states (`CLINT`,
//! `PLIC`), UART, VirtIO transport rings, and non-zero 4 KiB guest RAM pages so
//! a booted multi-core Linux VM can be restored in `< 50 ms`.

use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::Path;

use rivemu_cpu_riscv::Priv;
use rivemu_virtio::VirtQueue;

use crate::virt::Machine;

const SNAPSHOT_MAGIC: &[u8; 8] = b"RIVSNAP1";

impl Machine {
    /// Saves the complete VM state (CPUs, devices, and non-zero 4 KiB RAM
    /// pages) to `path`.
    pub fn save_snapshot(&self, path: &Path) -> io::Result<()> {
        let mut w = BufWriter::with_capacity(256 * 1024, File::create(path)?);
        w.write_all(SNAPSHOT_MAGIC)?;
        write_u64(&mut w, self.ram.len())?;
        write_u32(&mut w, self.harts.len() as u32)?;
        write_u64(&mut w, self.now_ticks())?;

        for h in &self.harts {
            write_u64(&mut w, h.pc)?;
            let flags = (h.prv as u8)
                | (u8::from(h.wfi) << 2)
                | (u8::from(h.handle_sbi) << 3)
                | (u8::from(h.hsm_stopped) << 4);
            w.write_all(&[flags, h.csr.fflags as u8, h.csr.frm as u8, 0])?;
            for &r in &h.x {
                write_u64(&mut w, r)?;
            }
            for &r in &h.f {
                write_u64(&mut w, r)?;
            }
            for v in [
                h.csr.mstatus,
                h.csr.medeleg,
                h.csr.mideleg,
                h.csr.mie,
                h.csr.mtvec,
                h.csr.mcounteren,
                h.csr.menvcfg,
                h.csr.mcountinhibit,
                h.csr.mscratch,
                h.csr.mepc,
                h.csr.mcause,
                h.csr.mtval,
                h.csr.mip,
                h.csr.stvec,
                h.csr.scounteren,
                h.csr.sscratch,
                h.csr.sepc,
                h.csr.scause,
                h.csr.stval,
                h.csr.satp,
                h.csr.stimecmp,
                h.csr.minstret,
            ] {
                write_u64(&mut w, v)?;
            }
            w.write_all(&h.csr.pmpcfg)?;
            for &addr in &h.csr.pmpaddr {
                write_u64(&mut w, addr)?;
            }
        }

        // CLINT
        let (msip, mtimecmp, clint_off) = self.clint.snapshot();
        write_bools(&mut w, &msip)?;
        write_u64s(&mut w, &mtimecmp)?;
        write_u64(&mut w, clint_off)?;

        // PLIC
        let (prio, level, pending, claimed, enable, thresh) = self.plic.snapshot();
        write_u32s(&mut w, &prio)?;
        write_bools(&mut w, &level)?;
        write_bools(&mut w, &pending)?;
        write_bools(&mut w, &claimed)?;
        write_u32(&mut w, enable.len() as u32)?;
        for ctx_en in &enable {
            write_u32s(&mut w, ctx_en)?;
        }
        write_u32s(&mut w, &thresh)?;

        // UART
        w.write_all(&self.uart.snapshot())?;

        // VirtIO transports
        write_u32(&mut w, self.virtio.len() as u32)?;
        for v in &self.virtio {
            let (regs, feats, queues) = v.snapshot();
            for r in regs {
                write_u32(&mut w, r)?;
            }
            write_u64(&mut w, feats)?;
            write_u32(&mut w, queues.len() as u32)?;
            for q in queues {
                write_u32(&mut w, q.num)?;
                w.write_all(&[u8::from(q.ready)])?;
                write_u16(&mut w, q.last_avail_idx)?;
                write_u64(&mut w, q.desc_addr)?;
                write_u64(&mut w, q.avail_addr)?;
                write_u64(&mut w, q.used_addr)?;
            }
        }

        // Sparse 4 KiB RAM pages (only pages touched since creation/reset).
        let non_zero = self.ram.dirty_non_zero_pages();
        write_u32s(&mut w, &non_zero)?;
        let mut page_buf = [0u8; 4096];
        for p in non_zero {
            self.ram.read_page(p, &mut page_buf);
            w.write_all(&page_buf)?;
        }
        w.flush()
    }

    /// Restores the complete VM state from a snapshot file created by
    /// [`Self::save_snapshot`].
    pub fn load_snapshot(&mut self, path: &Path) -> io::Result<()> {
        let mut r = BufReader::with_capacity(256 * 1024, File::open(path)?);
        let mut magic = [0u8; 8];
        r.read_exact(&mut magic)?;
        if &magic != SNAPSHOT_MAGIC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid rivemu snapshot magic",
            ));
        }
        let ram_size = read_u64(&mut r)?;
        if ram_size != self.ram.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "snapshot RAM size ({ram_size}) does not match machine RAM size ({})",
                    self.ram.len()
                ),
            ));
        }
        let num_harts = read_u32(&mut r)? as usize;
        if num_harts != self.harts.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "snapshot SMP count ({num_harts}) does not match machine SMP count ({})",
                    self.harts.len()
                ),
            ));
        }
        let saved_ticks = read_u64(&mut r)?;
        self.timebase.set_now(saved_ticks);
        if let Some(ic) = &self.icount {
            ic.set_now(saved_ticks);
        }

        for h in &mut self.harts {
            h.pc = read_u64(&mut r)?;
            let mut hdr = [0u8; 4];
            r.read_exact(&mut hdr)?;
            h.prv = match hdr[0] & 3 {
                0 => Priv::User,
                1 => Priv::Supervisor,
                _ => Priv::Machine,
            };
            h.wfi = hdr[0] & (1 << 2) != 0;
            h.handle_sbi = hdr[0] & (1 << 3) != 0;
            h.hsm_stopped = hdr[0] & (1 << 4) != 0;
            h.csr.fflags = u64::from(hdr[1]);
            h.csr.frm = u64::from(hdr[2]);
            for reg in &mut h.x {
                *reg = read_u64(&mut r)?;
            }
            for reg in &mut h.f {
                *reg = read_u64(&mut r)?;
            }
            h.csr.mstatus = read_u64(&mut r)?;
            h.csr.medeleg = read_u64(&mut r)?;
            h.csr.mideleg = read_u64(&mut r)?;
            h.csr.mie = read_u64(&mut r)?;
            h.csr.mtvec = read_u64(&mut r)?;
            h.csr.mcounteren = read_u64(&mut r)?;
            h.csr.menvcfg = read_u64(&mut r)?;
            h.csr.mcountinhibit = read_u64(&mut r)?;
            h.csr.mscratch = read_u64(&mut r)?;
            h.csr.mepc = read_u64(&mut r)?;
            h.csr.mcause = read_u64(&mut r)?;
            h.csr.mtval = read_u64(&mut r)?;
            h.csr.mip = read_u64(&mut r)?;
            h.csr.stvec = read_u64(&mut r)?;
            h.csr.scounteren = read_u64(&mut r)?;
            h.csr.sscratch = read_u64(&mut r)?;
            h.csr.sepc = read_u64(&mut r)?;
            h.csr.scause = read_u64(&mut r)?;
            h.csr.stval = read_u64(&mut r)?;
            h.csr.satp = read_u64(&mut r)?;
            h.csr.stimecmp = read_u64(&mut r)?;
            h.csr.minstret = read_u64(&mut r)?;
            r.read_exact(&mut h.csr.pmpcfg)?;
            for addr in &mut h.csr.pmpaddr {
                *addr = read_u64(&mut r)?;
            }
            h.pending_sbi = false;
            h.pending_semihost = false;
            h.invalidate_tlb();
            h.invalidate_icache();
            h.notify_irq();
        }

        // CLINT
        let msip = read_bools(&mut r)?;
        let mtimecmp = read_u64s(&mut r)?;
        let clint_off = read_u64(&mut r)?;
        self.clint.restore(msip, mtimecmp, clint_off);

        // PLIC
        let prio = read_u32s(&mut r)?;
        let level = read_bools(&mut r)?;
        let pending = read_bools(&mut r)?;
        let claimed = read_bools(&mut r)?;
        let en_len = read_u32(&mut r)? as usize;
        let mut enable = Vec::with_capacity(en_len);
        for _ in 0..en_len {
            enable.push(read_u32s(&mut r)?);
        }
        let thresh = read_u32s(&mut r)?;
        self.plic
            .restore(prio, level, pending, claimed, enable, thresh);

        // UART
        let mut uart_regs = [0u8; 8];
        r.read_exact(&mut uart_regs)?;
        self.uart.restore(uart_regs);

        // VirtIO transports
        let virtio_count = read_u32(&mut r)? as usize;
        if virtio_count != self.virtio.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "snapshot virtio device count mismatch",
            ));
        }
        for v in &self.virtio {
            let mut regs = [0u32; 6];
            for reg in &mut regs {
                *reg = read_u32(&mut r)?;
            }
            let feats = read_u64(&mut r)?;
            let qlen = read_u32(&mut r)? as usize;
            let mut queues = Vec::with_capacity(qlen);
            for _ in 0..qlen {
                let num = read_u32(&mut r)?;
                let mut ready = [0u8; 1];
                r.read_exact(&mut ready)?;
                let last_avail_idx = read_u16(&mut r)?;
                let desc_addr = read_u64(&mut r)?;
                let avail_addr = read_u64(&mut r)?;
                let used_addr = read_u64(&mut r)?;
                queues.push(VirtQueue {
                    num,
                    ready: ready[0] != 0,
                    desc_addr,
                    avail_addr,
                    used_addr,
                    last_avail_idx,
                });
            }
            v.restore(regs, feats, queues);
        }

        // Sparse 4 KiB RAM pages: zero any dirty pages not in the snapshot, then bulk-copy saved pages.
        let pages = read_u32s(&mut r)?;
        self.ram.reset_dirty_for_restore(&pages);
        let mut page_buf = [0u8; 4096];
        for p in pages {
            r.read_exact(&mut page_buf)?;
            self.ram.write_page(p, &page_buf);
        }
        Ok(())
    }
}

fn write_u16(w: &mut impl Write, v: u16) -> io::Result<()> {
    w.write_all(&v.to_le_bytes())
}

fn write_u32(w: &mut impl Write, v: u32) -> io::Result<()> {
    w.write_all(&v.to_le_bytes())
}

fn write_u64(w: &mut impl Write, v: u64) -> io::Result<()> {
    w.write_all(&v.to_le_bytes())
}

fn write_bools(w: &mut impl Write, s: &[bool]) -> io::Result<()> {
    write_u32(w, s.len() as u32)?;
    let bytes: Vec<u8> = s.iter().map(|&b| u8::from(b)).collect();
    w.write_all(&bytes)
}

fn write_u32s(w: &mut impl Write, s: &[u32]) -> io::Result<()> {
    write_u32(w, s.len() as u32)?;
    for &v in s {
        write_u32(w, v)?;
    }
    Ok(())
}

fn write_u64s(w: &mut impl Write, s: &[u64]) -> io::Result<()> {
    write_u32(w, s.len() as u32)?;
    for &v in s {
        write_u64(w, v)?;
    }
    Ok(())
}

fn read_u16(r: &mut impl Read) -> io::Result<u16> {
    let mut b = [0u8; 2];
    r.read_exact(&mut b)?;
    Ok(u16::from_le_bytes(b))
}

fn read_u32(r: &mut impl Read) -> io::Result<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_le_bytes(b))
}

fn read_u64(r: &mut impl Read) -> io::Result<u64> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b)?;
    Ok(u64::from_le_bytes(b))
}

fn read_bools(r: &mut impl Read) -> io::Result<Vec<bool>> {
    let len = read_u32(r)? as usize;
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    Ok(buf.into_iter().map(|b| b != 0).collect())
}

fn read_u32s(r: &mut impl Read) -> io::Result<Vec<u32>> {
    let len = read_u32(r)? as usize;
    let mut out = Vec::with_capacity(len);
    for _ in 0..len {
        out.push(read_u32(r)?);
    }
    Ok(out)
}

fn read_u64s(r: &mut impl Read) -> io::Result<Vec<u64>> {
    let len = read_u32(r)? as usize;
    let mut out = Vec::with_capacity(len);
    for _ in 0..len {
        out.push(read_u64(r)?);
    }
    Ok(out)
}
