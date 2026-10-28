//! The RISC-V `virt` board.
//!
//! Memory map and image placement follow QEMU's `hw/riscv/virt.c` so that
//! firmware, kernels and device trees built for QEMU work unchanged.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rivemu_backends::{BlockBackend, Chardev, NetBackend};
use rivemu_core::{
    AccessSize, GuestAddr, HostTimebase, IcountTimebase, Timebase, align_down, align_up, size::MIB,
};
use rivemu_cpu_riscv::csr::{envcfg, irq};
use rivemu_cpu_riscv::{Hart, HartIrqs, Logger};
use rivemu_devices::{Clint, GoldfishRtc, Plic, PowerControl, PowerEvent, SifiveTest, Uart16550};
use rivemu_mem::{AddressSpace, MapError, Ram};
use rivemu_virtio::{
    DmaRam, Virtio9p, VirtioBlk, VirtioConsole, VirtioDevice, VirtioMmio, VirtioNet, VirtioRng,
};
use thiserror::Error;

use crate::fdt::{self, FdtParams};
use crate::loader::{self, LoadError};

/// Physical memory map of the `virt` board (base, size).
pub mod memmap {
    use rivemu_core::GuestAddr;

    /// Mask ROM holding the reset vector.
    pub const MROM: (GuestAddr, u64) = (0x0000_1000, 0xf000);
    /// SiFive test device (poweroff / reboot).
    pub const TEST: (GuestAddr, u64) = (0x0010_0000, 0x1000);
    /// Goldfish RTC.
    pub const RTC: (GuestAddr, u64) = (0x0010_1000, 0x1000);
    /// CLINT / ACLINT MTIMER + MSWI.
    pub const CLINT: (GuestAddr, u64) = (0x0200_0000, 0x1_0000);
    /// PLIC.
    pub const PLIC: (GuestAddr, u64) = (0x0c00_0000, 0x0060_0000);
    /// NS16550A UART.
    pub const UART0: (GuestAddr, u64) = (0x1000_0000, 0x100);
    /// First virtio-mmio transport; 8 transports at 0x1000 stride.
    pub const VIRTIO: (GuestAddr, u64) = (0x1000_1000, 0x1000);
    /// Number of virtio-mmio transports.
    pub const VIRTIO_COUNT: u64 = 8;
    /// Start of main memory.
    pub const DRAM_BASE: GuestAddr = 0x8000_0000;
}

/// Default RAM size (QEMU's default for `-machine virt`).
pub const DEFAULT_RAM_SIZE: u64 = 128 * MIB;
/// Frequency of `mtime` / the `time` CSR (same as QEMU).
pub const TIMEBASE_FREQ: u64 = 10_000_000;
/// PLIC source of the UART.
pub const UART0_IRQ: u32 = 10;
/// PLIC source of the RTC.
pub const RTC_IRQ: u32 = 11;
/// PLIC source of the first virtio-mmio transport.
pub const VIRTIO_IRQ: u32 = 1;
/// Number of PLIC sources (including the reserved source 0).
pub const PLIC_NUM_SOURCES: usize = 96;
/// Instructions each hart executes per scheduling slice.
pub const QUANTUM: u64 = 10_000;
/// Longest time the machine loop sleeps while all harts are idle.
pub const MAX_IDLE: Duration = Duration::from_millis(10);

/// Maximum number of harts supported by the board.
pub const MAX_HARTS: u32 = 512;

/// Specification of a `virtio-mmio` device to attach to the `virt` board.
#[derive(Debug, Clone)]
pub enum VirtioDeviceConfig {
    /// `virtio-blk-device`.
    Blk {
        /// Storage backend.
        backend: Arc<dyn BlockBackend>,
        /// Volume serial string.
        serial: String,
    },
    /// `virtio-net-device`.
    Net {
        /// Network backend.
        backend: Arc<dyn NetBackend>,
    },
    /// `virtio-serial-device` (`virtio-console`).
    Console {
        /// Character backend.
        chardev: Arc<dyn Chardev>,
    },
    /// `virtio-rng-device`.
    Rng,
    /// `virtio-9p-device` (`9P2000.L` host directory share).
    P9 {
        /// Host root directory path.
        path: PathBuf,
        /// Guest mount tag.
        mount_tag: String,
    },
}

/// Board configuration (already resolved from the command line).
#[derive(Debug, Clone)]
pub struct MachineConfig {
    /// RAM size in bytes.
    pub ram_size: u64,
    /// Number of harts.
    pub smp: u32,
    /// Firmware image (OpenSBI), loaded at the start of DRAM. `None` = `-bios none`.
    pub bios: Option<PathBuf>,
    /// Kernel image (ELF or raw).
    pub kernel: Option<PathBuf>,
    /// Initial ramdisk.
    pub initrd: Option<PathBuf>,
    /// User supplied device tree blob.
    pub dtb: Option<PathBuf>,
    /// Kernel command line.
    pub append: Option<String>,
    /// Attached `virtio-mmio` devices (up to [`memmap::VIRTIO_COUNT`]).
    pub virtio: Vec<VirtioDeviceConfig>,
    /// Deterministic instruction-count timing shift (`-icount [shift=]N`).
    pub icount: Option<u8>,
}

impl Default for MachineConfig {
    fn default() -> Self {
        Self {
            ram_size: DEFAULT_RAM_SIZE,
            smp: 1,
            bios: None,
            kernel: None,
            initrd: None,
            dtb: None,
            append: None,
            virtio: Vec::new(),
            icount: None,
        }
    }
}

/// Board construction error.
#[derive(Debug, Error)]
pub enum MachineError {
    /// Invalid configuration.
    #[error("{0}")]
    Config(String),
    /// RAM allocation failed.
    #[error("cannot allocate {size} bytes of guest RAM: {source}")]
    Ram {
        /// Requested size.
        size: u64,
        /// OS error.
        source: std::io::Error,
    },
    /// Reading an image file failed.
    #[error("could not read {what} '{}': {source}", path.display())]
    Io {
        /// Kind of image (firmware, kernel, ...).
        what: &'static str,
        /// File path.
        path: PathBuf,
        /// OS error.
        source: std::io::Error,
    },
    /// Loading an image into guest memory failed.
    #[error("could not load {what} '{}': {source}", path.display())]
    Load {
        /// Kind of image.
        what: &'static str,
        /// File path.
        path: PathBuf,
        /// Loader error.
        source: LoadError,
    },
    /// Building the address space failed.
    #[error(transparent)]
    Map(#[from] MapError),
}

/// Where an image ended up in guest memory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageInfo {
    /// Entry point.
    pub entry: GuestAddr,
    /// First byte.
    pub start: GuestAddr,
    /// One past the last byte.
    pub end: GuestAddr,
    /// Whether the image was an ELF file.
    pub elf: bool,
    /// Whether the image carries a RISC-V Linux `Image` header (`RSC\x05`).
    pub linux_image: bool,
    /// ELF symbol table (`name -> address`), empty for stripped or raw images.
    pub symbols: HashMap<String, u64>,
}

/// Result of image placement; consumed by the reset vector / FDT code.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BootInfo {
    /// Address the boot hart jumps to after the reset vector.
    pub entry: GuestAddr,
    /// Firmware placement.
    pub firmware: Option<ImageInfo>,
    /// Kernel placement.
    pub kernel: Option<ImageInfo>,
    /// Initrd `[start, end)`.
    pub initrd: Option<(GuestAddr, GuestAddr)>,
    /// User supplied device tree blob, if any.
    pub dtb: Option<Vec<u8>>,
}

/// An assembled `virt` machine.
#[derive(Debug)]
pub struct Machine {
    /// Configuration it was built from.
    pub config: MachineConfig,
    /// Guest physical address space.
    pub bus: AddressSpace,
    /// Main memory.
    pub ram: Arc<Ram>,
    /// Image placement.
    pub boot: BootInfo,
    /// Guest physical address of the device tree.
    pub fdt_addr: GuestAddr,
    /// The harts.
    pub harts: Vec<Hart>,
    /// Console UART.
    pub uart: Arc<Uart16550>,
    /// Timer / software interrupt controller.
    pub clint: Arc<Clint>,
    /// External interrupt controller.
    pub plic: Arc<Plic>,
    /// Guest power requests.
    pub power: Arc<PowerControl>,
    /// Attached `virtio-mmio` transports.
    pub virtio: Vec<Arc<VirtioMmio>>,
    /// Host monotonic timebase.
    pub timebase: Arc<HostTimebase>,
    /// Instruction-counted timebase when `-icount` is enabled.
    pub icount: Option<Arc<IcountTimebase>>,
}

fn read_file(what: &'static str, path: &Path) -> Result<Vec<u8>, MachineError> {
    std::fs::read(path).map_err(|source| MachineError::Io {
        what,
        path: path.to_owned(),
        source,
    })
}

fn load_image(
    bus: &AddressSpace,
    what: &'static str,
    path: &Path,
    raw_addr: GuestAddr,
) -> Result<ImageInfo, MachineError> {
    let data = read_file(what, path)?;
    let wrap = |source| MachineError::Load {
        what,
        path: path.to_owned(),
        source,
    };
    if loader::is_elf(&data) {
        let img = loader::load_elf(bus, &data).map_err(wrap)?;
        Ok(ImageInfo {
            entry: img.entry,
            start: img.low,
            end: img.high,
            elf: true,
            linux_image: false,
            symbols: img.symbols,
        })
    } else {
        let linux_image =
            data.len() >= 64 && (&data[48..56] == b"RISCV\0\0\0" || &data[56..60] == b"RSC\x05");
        let load_addr = if linux_image {
            let text_offset =
                u64::from_le_bytes(data[8..16].try_into().expect("8-byte slice")).min(64 * MIB);
            raw_addr.max(memmap::DRAM_BASE + text_offset)
        } else {
            raw_addr
        };
        let (start, end) = loader::load_raw(bus, load_addr, &data).map_err(wrap)?;
        Ok(ImageInfo {
            entry: start,
            start,
            end,
            elf: false,
            linux_image,
            symbols: HashMap::new(),
        })
    }
}

impl Machine {
    /// Builds the board and loads all images.
    pub fn new(config: MachineConfig) -> Result<Self, MachineError> {
        if config.smp == 0 || config.smp > MAX_HARTS {
            return Err(MachineError::Config(format!(
                "invalid number of CPUs {} (supported: 1..={MAX_HARTS})",
                config.smp
            )));
        }
        if config.kernel.is_none() {
            if config.initrd.is_some() {
                return Err(MachineError::Config(
                    "-initrd only allowed with -kernel option".into(),
                ));
            }
            if config.append.is_some() {
                return Err(MachineError::Config(
                    "-append only allowed with -kernel option".into(),
                ));
            }
        }

        let ram = Arc::new(
            Ram::new(config.ram_size).map_err(|source| MachineError::Ram {
                size: config.ram_size,
                source,
            })?,
        );
        let mut bus = AddressSpace::new();
        bus.add_ram("riscv_virt_board.ram", memmap::DRAM_BASE, ram.clone())?;
        let ram_end = memmap::DRAM_BASE + config.ram_size;

        // Harts and devices.
        let timebase = Arc::new(HostTimebase::new(TIMEBASE_FREQ));
        let icount = config
            .icount
            .map(|shift| Arc::new(IcountTimebase::new(TIMEBASE_FREQ, shift)));
        let tb: Arc<dyn Timebase> = match &icount {
            Some(ic) => ic.clone(),
            None => timebase.clone(),
        };
        let mut harts: Vec<Hart> = (0..config.smp)
            .map(|i| Hart::new(u64::from(i), tb.clone()))
            .collect();
        let lines: Vec<Arc<HartIrqs>> = harts.iter().map(|h| h.irqs.clone()).collect();
        let clint = Arc::new(Clint::new(tb, lines.clone()));
        let plic = Arc::new(Plic::new(PLIC_NUM_SOURCES, lines));
        let uart = Arc::new(Uart16550::new(plic.line(UART0_IRQ)));
        let power = PowerControl::new();
        let mrom = Arc::new(
            Ram::new(memmap::MROM.1).map_err(|source| MachineError::Ram {
                size: memmap::MROM.1,
                source,
            })?,
        );
        bus.add_rom("riscv_virt_board.mrom", memmap::MROM.0, mrom)?;
        let (b, s) = memmap::TEST;
        bus.add_mmio(
            "riscv.sifive.test",
            b,
            s,
            Arc::new(SifiveTest::new(power.clone())),
        )?;
        let (b, s) = memmap::RTC;
        bus.add_mmio("goldfish_rtc", b, s, Arc::new(GoldfishRtc::new()))?;
        let (b, s) = memmap::CLINT;
        bus.add_mmio("riscv.sifive.clint", b, s, clint.clone())?;
        let (b, s) = memmap::PLIC;
        bus.add_mmio("riscv.sifive.plic", b, s, plic.clone())?;
        let (b, s) = memmap::UART0;
        bus.add_mmio("serial", b, s, uart.clone())?;

        if config.virtio.len() as u64 > memmap::VIRTIO_COUNT {
            return Err(MachineError::Config(format!(
                "too many virtio devices ({}; maximum is {})",
                config.virtio.len(),
                memmap::VIRTIO_COUNT
            )));
        }
        let dma = DmaRam::new(memmap::DRAM_BASE, ram.clone());
        let mut virtio = Vec::with_capacity(config.virtio.len());
        let mut virtio_fdt = Vec::with_capacity(config.virtio.len());
        for (idx, vcfg) in config.virtio.iter().enumerate() {
            let base = memmap::VIRTIO.0 + idx as u64 * memmap::VIRTIO.1;
            let irq = VIRTIO_IRQ + idx as u32;
            let dev: Box<dyn VirtioDevice> = match vcfg {
                VirtioDeviceConfig::Blk { backend, serial } => {
                    Box::new(VirtioBlk::new(backend.clone(), serial))
                }
                VirtioDeviceConfig::Net { backend } => Box::new(VirtioNet::new(backend.clone())),
                VirtioDeviceConfig::Console { chardev } => {
                    Box::new(VirtioConsole::new(chardev.clone()))
                }
                VirtioDeviceConfig::Rng => Box::new(VirtioRng::default()),
                VirtioDeviceConfig::P9 { path, mount_tag } => {
                    Box::new(Virtio9p::new(path, mount_tag))
                }
            };
            let mmio = VirtioMmio::new(dev, dma.clone(), plic.line(irq));
            bus.add_mmio(
                &format!("virtio-mmio.{idx}"),
                base,
                memmap::VIRTIO.1,
                mmio.clone(),
            )?;
            virtio.push(mmio);
            virtio_fdt.push((base, irq));
        }

        let mut boot = BootInfo::default();

        if let Some(path) = &config.bios {
            boot.firmware = Some(load_image(&bus, "firmware", path, memmap::DRAM_BASE)?);
        }

        if let Some(path) = &config.kernel {
            // QEMU: on RV64 the kernel goes to the next 2 MiB boundary after
            // the firmware (or to the start of DRAM without firmware).
            let raw_addr = match &boot.firmware {
                Some(fw) => align_up(fw.end, 2 * MIB),
                None => memmap::DRAM_BASE,
            };
            let kernel = load_image(&bus, "kernel", path, raw_addr)?;

            if let Some(initrd_path) = &config.initrd {
                let raw_data = read_file("initrd", initrd_path)?;
                let data = if loader::is_elf(&raw_data) {
                    crate::cpio::build_initramfs(&raw_data)
                } else {
                    raw_data
                };
                // QEMU: halfway into RAM for small boards, 128 MiB past the
                // kernel otherwise.
                let start = kernel.start + (config.ram_size / 2).min(128 * MIB);
                let end = start + data.len() as u64;
                if start < kernel.end || end > ram_end {
                    return Err(MachineError::Config(format!(
                        "initrd '{}' ({} bytes) does not fit into guest RAM; increase -m",
                        initrd_path.display(),
                        data.len()
                    )));
                }
                loader::load_raw(&bus, start, &data).map_err(|source| MachineError::Load {
                    what: "initrd",
                    path: initrd_path.clone(),
                    source,
                })?;
                boot.initrd = Some((start, end));
            }
            boot.kernel = Some(kernel);
        }

        if let Some(path) = &config.dtb {
            boot.dtb = Some(read_file("device tree", path)?);
        }

        boot.entry = boot
            .firmware
            .as_ref()
            .or(boot.kernel.as_ref())
            .map_or(memmap::DRAM_BASE, |img| img.entry);

        // Device tree: user supplied or generated, placed at the end of
        // (low) DRAM like QEMU does.
        let fdt = match &boot.dtb {
            Some(d) => d.clone(),
            None => fdt::build(&FdtParams {
                harts: config.smp,
                ram_size: config.ram_size,
                bootargs: config.append.as_deref(),
                initrd: boot.initrd,
                plic_ndev: PLIC_NUM_SOURCES as u32 - 1,
                virtio: virtio_fdt,
            }),
        };
        let fdt_top = ram_end.min(3072 * MIB);
        let fdt_addr = align_down(fdt_top - fdt.len() as u64, 2 * MIB);
        let used_end = [boot.firmware.as_ref(), boot.kernel.as_ref()]
            .into_iter()
            .flatten()
            .map(|i| i.end)
            .chain(boot.initrd.map(|(_, e)| e))
            .max()
            .unwrap_or(memmap::DRAM_BASE);
        if fdt_addr < used_end {
            return Err(MachineError::Config(
                "not enough guest RAM for the device tree; increase -m".into(),
            ));
        }
        bus.load_bytes(fdt_addr, &fdt)
            .map_err(|e| MachineError::Config(format!("cannot place device tree: {e}")))?;

        // Reset vector + fw_dynamic info in the mask ROM.
        let next_addr = boot.kernel.as_ref().map_or(0, |k| k.entry);
        bus.load_bytes(
            memmap::MROM.0,
            &reset_vector(boot.entry, fdt_addr, next_addr),
        )
        .expect("mask ROM is mapped");
        for h in &mut harts {
            h.reset(memmap::MROM.0);
        }

        // If booting a RISC-V Linux `Image` or an S-mode kernel ELF linked at
        // `>= DRAM_BASE + 2 MiB` without external firmware (`-bios none` /
        // `-bios builtin`), boot directly into S-mode using rivemu's built-in
        // pure-Rust SBI v2.0 handler.
        let smode_direct = boot.firmware.is_none()
            && boot.kernel.as_ref().is_some_and(|k| {
                k.linux_image || (k.elf && k.start >= memmap::DRAM_BASE + 2 * MIB)
            });
        if smode_direct {
            harts[0].init_smode_boot(boot.entry, fdt_addr);
            for h in &mut harts[1..] {
                h.handle_sbi = true;
                h.hsm_stopped = true;
                h.wfi = true;
            }
        }

        Ok(Self {
            config,
            bus,
            ram,
            boot,
            fdt_addr,
            harts,
            uart,
            clint,
            plic,
            power,
            virtio,
            timebase,
            icount,
        })
    }

    /// Runs every hart for one scheduling slice. Returns a pending power
    /// event (poweroff / reset) requested by the guest, if any.
    pub fn run_slice(&mut self) -> Option<PowerEvent> {
        self.clint.update();
        for v in &self.virtio {
            v.poll();
        }
        for i in 0..self.harts.len() {
            let mut rem = QUANTUM;
            while rem > 0 {
                let ran = self.harts[i].run(&self.bus, rem);
                if let Some(ic) = &self.icount {
                    ic.advance(ran);
                }
                rem = rem.saturating_sub(ran);
                if self.harts[i].pending_sbi {
                    self.harts[i].pending_sbi = false;
                    if self.handle_sbi_call(i) {
                        break;
                    }
                } else if self.harts[i].pending_semihost {
                    self.harts[i].pending_semihost = false;
                    if self.handle_semihost_call(i) {
                        break;
                    }
                } else {
                    break;
                }
            }
        }
        if let Some((ic, d)) = self
            .icount
            .as_ref()
            .filter(|_| self.all_idle())
            .zip(self.next_timer_deadline())
        {
            ic.warp_to(d);
            self.clint.update();
        }
        self.power.take()
    }

    /// Services a machine-wide SBI v2.0 call from `hart_idx`. Returns `true`
    /// if the calling hart should yield the remainder of its scheduling slice.
    fn handle_sbi_call(&mut self, hart_idx: usize) -> bool {
        let eid = self.harts[hart_idx].x[17];
        let fid = self.harts[hart_idx].x[16];
        let a0 = self.harts[hart_idx].x[10];
        let a1 = self.harts[hart_idx].x[11];
        let a2 = self.harts[hart_idx].x[12];
        let num_harts = self.harts.len();

        let target_harts = |mask: u64, base: u64| -> Result<Vec<usize>, u64> {
            if base == u64::MAX {
                return Ok((0..num_harts).collect());
            }
            if base >= num_harts as u64 {
                return Err((-3i64) as u64);
            }
            let mut out = Vec::new();
            for bit in 0..64u64 {
                if (mask >> bit) & 1 != 0 {
                    let h = base.saturating_add(bit) as usize;
                    if h < num_harts {
                        out.push(h);
                    }
                }
            }
            Ok(out)
        };

        let mut yield_slice = false;
        let (err, val): (u64, u64) = match (eid, fid) {
            // IPI extension (sPI) and legacy send_ipi.
            (0x0073_5049, 0) => match target_harts(a0, a1) {
                Ok(targets) => {
                    for t in targets {
                        self.harts[t].csr.mip |= irq::SSIP;
                        self.harts[t].notify_irq();
                    }
                    yield_slice = true;
                    (0, 0)
                }
                Err(e) => (e, 0),
            },
            (0x04, _) => {
                for h in &mut self.harts {
                    h.csr.mip |= irq::SSIP;
                    h.notify_irq();
                }
                yield_slice = true;
                (0, 0)
            }
            // RFENCE extension and legacy remote fence calls.
            (0x5246_4e43, 0..=6) => match target_harts(a0, a1) {
                Ok(targets) => {
                    for t in targets {
                        if fid == 0 {
                            self.harts[t].invalidate_icache();
                        } else {
                            self.harts[t].invalidate_tlb();
                        }
                    }
                    (0, 0)
                }
                Err(e) => (e, 0),
            },
            (0x05, _) => {
                for h in &mut self.harts {
                    h.invalidate_icache();
                }
                (0, 0)
            }
            (0x06 | 0x07, _) => {
                for h in &mut self.harts {
                    h.invalidate_tlb();
                }
                (0, 0)
            }
            // Hart State Management (HSM) extension.
            (0x0048_534d, 0) => {
                let target = a0 as usize;
                if target >= num_harts {
                    ((-3i64) as u64, 0)
                } else if !self.harts[target].hsm_stopped {
                    ((-6i64) as u64, 0)
                } else {
                    self.harts[target].init_smode_boot(a1, a2);
                    yield_slice = true;
                    (0, 0)
                }
            }
            (0x0048_534d, 1) => {
                self.harts[hart_idx].hsm_stopped = true;
                self.harts[hart_idx].wfi = true;
                yield_slice = true;
                (0, 0)
            }
            (0x0048_534d, 2) => {
                let target = a0 as usize;
                if target >= num_harts {
                    ((-3i64) as u64, 0)
                } else {
                    (0, u64::from(self.harts[target].hsm_stopped))
                }
            }
            (0x0048_534d, 3) => {
                let suspend_type = a0 as u32;
                if suspend_type == 0 {
                    self.harts[hart_idx].wfi = true;
                    self.harts[hart_idx].notify_irq();
                    yield_slice = true;
                    (0, 0)
                } else if suspend_type & 0x8000_0000 != 0 {
                    self.harts[hart_idx].init_smode_boot(a1, a2);
                    self.harts[hart_idx].wfi = true;
                    self.harts[hart_idx].notify_irq();
                    return true;
                } else {
                    ((-3i64) as u64, 0)
                }
            }
            // System Reset (SRST) extension and legacy shutdown.
            (0x5352_5354, 0) => {
                match a0 as u32 {
                    0 => self.power.request(PowerEvent::Poweroff(0)),
                    1 | 2 => self.power.request(PowerEvent::Reset),
                    _ => return false,
                }
                for h in &mut self.harts {
                    h.hsm_stopped = true;
                    h.wfi = true;
                }
                yield_slice = true;
                (0, 0)
            }
            (0x08, _) => {
                self.power.request(PowerEvent::Poweroff(0));
                for h in &mut self.harts {
                    h.hsm_stopped = true;
                    h.wfi = true;
                }
                yield_slice = true;
                (0, 0)
            }
            // Debug Console (DBCN) and legacy console putchar/getchar.
            (0x4442_434e, 0) => {
                if a2 != 0 {
                    ((-3i64) as u64, 0)
                } else {
                    let mut written = 0u64;
                    for off in 0..a0 {
                        match self.bus.read(a1.wrapping_add(off), AccessSize::Byte) {
                            Ok(b) => {
                                let _ = self.bus.write(memmap::UART0.0, AccessSize::Byte, b);
                                written += 1;
                            }
                            Err(_) => break,
                        }
                    }
                    (0, written)
                }
            }
            (0x4442_434e, 1) => (0, 0),
            (0x4442_434e, 2) | (0x01, _) => {
                let _ = self.bus.write(memmap::UART0.0, AccessSize::Byte, a0 & 0xff);
                (0, 0)
            }
            (0x02, _) => (u64::MAX, 0),
            _ => ((-2i64) as u64, 0),
        };
        self.harts[hart_idx].x[10] = err;
        self.harts[hart_idx].x[11] = val;
        self.harts[hart_idx].log_sbi(eid, fid, a0, a1, err, val);
        yield_slice
    }

    /// Services a RISC-V Semihosting call (`slli x0, x0, 0x1f; ebreak; srai x0, x0, 7`)
    /// from `hart_idx`. Returns `true` if the hart should yield its slice.
    fn handle_semihost_call(&mut self, hart_idx: usize) -> bool {
        let op = self.harts[hart_idx].x[10];
        let param = self.harts[hart_idx].x[11];
        let read_u8 = |va: u64| -> Option<u8> {
            let pa = self.harts[hart_idx].debug_translate(&self.bus, va)?;
            self.bus.read(pa, AccessSize::Byte).ok().map(|v| v as u8)
        };
        let read_u64 = |va: u64| -> Option<u64> {
            let pa = self.harts[hart_idx].debug_translate(&self.bus, va)?;
            self.bus.read(pa, AccessSize::Double).ok()
        };

        match op {
            // SYS_WRITEC (0x03): param points to a single character byte.
            0x03 => {
                if let Some(b) = read_u8(param) {
                    let _ = self
                        .bus
                        .write(memmap::UART0.0, AccessSize::Byte, u64::from(b));
                }
                self.harts[hart_idx].x[10] = 0;
                false
            }
            // SYS_WRITE0 (0x04): param points to a null-terminated string.
            0x04 => {
                for off in 0..4096u64 {
                    match read_u8(param.wrapping_add(off)) {
                        Some(0) | None => break,
                        Some(b) => {
                            let _ = self
                                .bus
                                .write(memmap::UART0.0, AccessSize::Byte, u64::from(b));
                        }
                    }
                }
                self.harts[hart_idx].x[10] = 0;
                false
            }
            // SYS_WRITE (0x05): param points to [fd, ptr, len].
            0x05 => {
                let ptr = read_u64(param.wrapping_add(8)).unwrap_or(0);
                let len = read_u64(param.wrapping_add(16)).unwrap_or(0).min(65536);
                let mut unwritten = len;
                for off in 0..len {
                    if let Some(b) = read_u8(ptr.wrapping_add(off)) {
                        let _ = self
                            .bus
                            .write(memmap::UART0.0, AccessSize::Byte, u64::from(b));
                        unwritten -= 1;
                    } else {
                        break;
                    }
                }
                self.harts[hart_idx].x[10] = unwritten;
                false
            }
            // SYS_EXIT (0x18) / SYS_EXIT_EXTENDED (0x20).
            0x18 | 0x20 => {
                const ADP_STOPPED_APPLICATION_EXIT: u64 = 0x20026;
                let code = if param == ADP_STOPPED_APPLICATION_EXIT {
                    0
                } else if let (Some(reason), Some(subcode)) =
                    (read_u64(param), read_u64(param.wrapping_add(8)))
                {
                    if reason == ADP_STOPPED_APPLICATION_EXIT {
                        subcode as i32
                    } else {
                        1
                    }
                } else {
                    1
                };
                self.power.request(PowerEvent::Poweroff(code));
                for h in &mut self.harts {
                    h.hsm_stopped = true;
                    h.wfi = true;
                }
                self.harts[hart_idx].x[10] = 0;
                true
            }
            _ => {
                self.harts[hart_idx].x[10] = u64::MAX;
                false
            }
        }
    }

    /// Whether all harts are waiting for an interrupt.
    pub fn all_idle(&self) -> bool {
        self.harts.iter().all(|h| h.wfi || h.hsm_stopped)
    }

    pub(crate) fn now_ticks(&self) -> u64 {
        match &self.icount {
            Some(ic) => ic.now(),
            None => self.timebase.now(),
        }
    }

    fn next_timer_deadline(&self) -> Option<u64> {
        let now = self.now_ticks();
        let stimecmp = self
            .harts
            .iter()
            .filter(|h| !h.hsm_stopped && h.csr.menvcfg & envcfg::STCE != 0 && h.csr.stimecmp > now)
            .map(|h| h.csr.stimecmp);
        self.clint.next_deadline().into_iter().chain(stimecmp).min()
    }

    /// How long the host may sleep before the next timer event.
    pub fn idle_timeout(&self) -> Duration {
        if self.icount.is_some() {
            return Duration::ZERO;
        }
        let now = self.now_ticks();
        match self.next_timer_deadline() {
            Some(d) => {
                Duration::from_nanos(self.timebase.ticks_to_ns(d.saturating_sub(now))).min(MAX_IDLE)
            }
            None => MAX_IDLE,
        }
    }

    /// Total instructions retired by all harts.
    pub fn instret(&self) -> u64 {
        self.harts.iter().map(Hart::instret).sum()
    }

    /// Attaches a diagnostic logger (`-d` / `-D`) to every hart and loads any
    /// ELF symbols from the firmware and kernel images for symbolication.
    pub fn set_logger(&mut self, logger: Option<Arc<Logger>>) {
        if let Some(l) = &logger {
            let mut syms = HashMap::new();
            if let Some(fw) = &self.boot.firmware {
                syms.extend(fw.symbols.iter().map(|(k, v)| (k.clone(), *v)));
            }
            if let Some(k) = &self.boot.kernel {
                syms.extend(k.symbols.iter().map(|(k, v)| (k.clone(), *v)));
            }
            if !syms.is_empty() {
                l.set_symbols(&syms);
            }
        }
        for h in &mut self.harts {
            h.set_logger(logger.clone());
        }
    }
}

/// QEMU-compatible reset vector followed by `fw_dynamic_info` (version 2).
fn reset_vector(start: GuestAddr, fdt_addr: GuestAddr, next_addr: GuestAddr) -> Vec<u8> {
    const CODE: [u32; 6] = [
        0x0000_0297, // auipc t0, 0
        0x0282_8613, // addi  a2, t0, 40   (&fw_dynamic_info)
        0xf140_2573, // csrr  a0, mhartid
        0x0202_b583, // ld    a1, 32(t0)   (fdt address)
        0x0182_b283, // ld    t0, 24(t0)   (start address)
        0x0002_8067, // jr    t0
    ];
    const FW_DYNAMIC_MAGIC: u64 = 0x4942_534f; // "OSBI"
    let mut v: Vec<u8> = CODE.iter().flat_map(|w| w.to_le_bytes()).collect();
    for word in [
        start,
        fdt_addr,
        FW_DYNAMIC_MAGIC,
        2,         // version
        next_addr, // next_addr
        1,         // next_mode: S-mode
        0,         // options
        0,         // boot_hart
    ] {
        v.extend_from_slice(&word.to_le_bytes());
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loader::tests::build_elf;
    use rivemu_core::AccessSize;
    use std::io::Write;

    struct TempFile(PathBuf);

    impl TempFile {
        fn new(tag: &str, data: &[u8]) -> Self {
            let path = std::env::temp_dir().join(format!(
                "rivemu-test-{}-{tag}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            std::fs::File::create(&path)
                .and_then(|mut f| f.write_all(data))
                .unwrap();
            Self(path)
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[test]
    fn empty_machine_boots_at_dram() {
        let m = Machine::new(MachineConfig::default()).unwrap();
        assert_eq!(m.boot.entry, memmap::DRAM_BASE);
        assert_eq!(m.ram.len(), DEFAULT_RAM_SIZE);
        assert_eq!(m.bus.read(memmap::DRAM_BASE, AccessSize::Word).unwrap(), 0);
    }

    #[test]
    fn firmware_kernel_initrd_layout() {
        let fw = TempFile::new("fw", &[0x11; 0x1234]);
        let kernel = TempFile::new("kernel", &[0x22; 0x100]);
        let initrd = TempFile::new("initrd", &[0x33; 0x10]);
        let m = Machine::new(MachineConfig {
            bios: Some(fw.0.clone()),
            kernel: Some(kernel.0.clone()),
            initrd: Some(initrd.0.clone()),
            append: Some("console=ttyS0".into()),
            ..Default::default()
        })
        .unwrap();
        let b = &m.boot;
        assert_eq!(b.entry, 0x8000_0000);
        assert_eq!(b.firmware.as_ref().unwrap().end, 0x8000_1234);
        let k = b.kernel.as_ref().unwrap();
        assert_eq!((k.start, k.entry, k.elf), (0x8020_0000, 0x8020_0000, false));
        assert_eq!(b.initrd, Some((0x8420_0000, 0x8420_0010)));
        assert_eq!(m.bus.read(0x8420_0000, AccessSize::Byte).unwrap(), 0x33);
    }

    #[test]
    fn elf_kernel_without_firmware() {
        let kernel = TempFile::new(
            "elf",
            &build_elf(0x8000_0000, 0x8000_0010, &[0; 32], 0, &[]),
        );
        let m = Machine::new(MachineConfig {
            kernel: Some(kernel.0.clone()),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(m.boot.entry, 0x8000_0010);
        assert!(m.boot.kernel.unwrap().elf);
    }

    #[test]
    fn config_errors() {
        let err = |cfg| Machine::new(cfg).unwrap_err().to_string();
        assert!(
            err(MachineConfig {
                smp: 0,
                ..Default::default()
            })
            .contains("invalid number of CPUs")
        );
        assert_eq!(
            err(MachineConfig {
                append: Some("x".into()),
                ..Default::default()
            }),
            "-append only allowed with -kernel option"
        );
        assert!(
            err(MachineConfig {
                kernel: Some("/nonexistent/rivemu/kernel".into()),
                ..Default::default()
            })
            .starts_with("could not read kernel")
        );
    }

    #[test]
    fn initrd_too_large() {
        let kernel = TempFile::new("k2", &[0; 16]);
        let initrd = TempFile::new("i2", &vec![0; 0x9000]);
        let e = Machine::new(MachineConfig {
            ram_size: 0x10000,
            kernel: Some(kernel.0.clone()),
            initrd: Some(initrd.0.clone()),
            ..Default::default()
        })
        .unwrap_err();
        assert!(e.to_string().contains("does not fit"), "{e}");
    }

    #[test]
    fn icount_deterministic_timer_warp() {
        let mut m = Machine::new(MachineConfig {
            icount: Some(0),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(m.idle_timeout(), Duration::ZERO);
        // Set mtimecmp[0] = 500_000 and enable MTIE on hart 0, then enter WFI.
        m.bus
            .write(memmap::CLINT.0 + 0x4000, AccessSize::Double, 500_000)
            .unwrap();
        m.harts[0].csr.mie = 1 << 7; // MTIE
        m.harts[0].wfi = true;
        m.run_slice();
        assert!(m.now_ticks() >= 500_000);
        m.run_slice();
        assert!(
            !m.harts[0].wfi,
            "hart 0 should wake after icount timer warp"
        );
    }

    #[test]
    fn smode_elf_boot_sbi_log_and_symbolicated_trap() {
        use rivemu_cpu_riscv::{LogFlags, Logger, Priv};
        use std::sync::Mutex;

        struct VecSink(Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for VecSink {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        // S-mode ELF linked at 0x8020_0000:
        //   li a7, 0x10; li a6, 0; ecall  (SBI get_spec_version)
        //   unimp (0x0000_0000 -> illegal instruction trap in S-mode)
        let code: Vec<u8> = [
            0x0100_0893u32, // li a7, 0x10
            0x0000_0813,    // li a6, 0
            0x0000_0073,    // ecall
            0x0000_0000,    // illegal instruction at _start + 12 (0xc)
        ]
        .iter()
        .flat_map(|w| w.to_le_bytes())
        .collect();
        let elf = build_elf(
            0x8020_0000,
            0x8020_0000,
            &code,
            0,
            &[("rust_os_entry", 0x8020_0000)],
        );
        let kernel = TempFile::new("smode_elf", &elf);
        let mut m = Machine::new(MachineConfig {
            kernel: Some(kernel.0.clone()),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(m.harts[0].prv, Priv::Supervisor);
        assert_eq!(m.harts[0].pc, 0x8020_0000);

        let buf = Arc::new(Mutex::new(Vec::new()));
        let logger = Logger::custom(
            LogFlags::from_items(["int", "sbi"]),
            Box::new(VecSink(buf.clone())),
        );
        m.set_logger(Some(logger));
        for _ in 0..4 {
            m.harts[0].step(&m.bus);
        }
        let log = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        assert!(
            log.contains("SBI [hart 0]: eid=0x10 fid=0x0"),
            "expected SBI log line, got:\n{log}"
        );
        assert!(
            log.contains("<rust_os_entry+0xc>"),
            "expected symbolicated trap PC <rust_os_entry+0xc>, got:\n{log}"
        );
    }

    #[test]
    fn riscv_semihosting_write0_and_exit() {
        // Semihosting sequence at 0x8000_0000:
        //   0x00: auipc a1, 0
        //   0x04: addi  a1, a1, 0x40    (points to "SEMI_OK\0")
        //   0x08: addi  a0, x0, 4       (SYS_WRITE0)
        //   0x0c: slli  x0, x0, 0x1f
        //   0x10: ebreak
        //   0x14: srai  x0, x0, 7
        //   0x18: lui   a1, 0x20        (0x20000)
        //   0x1c: addi  a1, a1, 0x26    (0x20026 = ADP_Stopped_ApplicationExit)
        //   0x20: addi  a0, x0, 0x18    (SYS_EXIT)
        //   0x24: slli  x0, x0, 0x1f
        //   0x28: ebreak
        //   0x2c: srai  x0, x0, 7
        let words: [u32; 12] = [
            0x0000_0597,
            0x0405_8593,
            0x0040_0513,
            0x01f0_1013,
            0x0010_0073,
            0x4070_5013,
            0x0002_05b7,
            0x0265_8593,
            0x0180_0513,
            0x01f0_1013,
            0x0010_0073,
            0x4070_5013,
        ];
        let mut raw = vec![0u8; 0x80];
        for (i, w) in words.iter().enumerate() {
            raw[i * 4..(i + 1) * 4].copy_from_slice(&w.to_le_bytes());
        }
        raw[0x40..0x48].copy_from_slice(b"SEMI_OK\0");
        let kernel = TempFile::new("semihost", &raw);
        let mut m = Machine::new(MachineConfig {
            kernel: Some(kernel.0.clone()),
            ..Default::default()
        })
        .unwrap();
        let ev = m.run_slice();
        assert_eq!(ev, Some(PowerEvent::Poweroff(0)));
        assert_eq!(m.uart.take_output(), b"SEMI_OK");
    }

    #[test]
    fn snapshot_save_and_restore_roundtrip() {
        let mut m1 = Machine::new(MachineConfig {
            smp: 2,
            ..Default::default()
        })
        .unwrap();
        m1.harts[0].pc = 0x8020_1234;
        m1.harts[0].x[10] = 0xdead_beef_cafe_babe;
        m1.harts[1].hsm_stopped = true;
        m1.bus
            .write(
                memmap::DRAM_BASE + 0x4000,
                AccessSize::Double,
                0x1122_3344_5566_7788,
            )
            .unwrap();

        let snap = TempFile::new("snap", &[]);
        m1.save_snapshot(&snap.0).unwrap();

        let mut m2 = Machine::new(MachineConfig {
            smp: 2,
            ..Default::default()
        })
        .unwrap();
        let t0 = std::time::Instant::now();
        m2.load_snapshot(&snap.0).unwrap();
        let elapsed = t0.elapsed();
        assert!(
            elapsed < Duration::from_millis(50),
            "expected snapshot restore in < 50 ms, took {elapsed:?}"
        );
        assert_eq!(m2.harts[0].pc, 0x8020_1234);
        assert_eq!(m2.harts[0].x[10], 0xdead_beef_cafe_babe);
        assert!(m2.harts[1].hsm_stopped);
        assert_eq!(
            m2.bus
                .read(memmap::DRAM_BASE + 0x4000, AccessSize::Double)
                .unwrap(),
            0x1122_3344_5566_7788
        );
    }
}
