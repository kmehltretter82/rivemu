//! The RISC-V `virt` board.
//!
//! Memory map and image placement follow QEMU's `hw/riscv/virt.c` so that
//! firmware, kernels and device trees built for QEMU work unchanged.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rivemu_backends::{BlockBackend, Chardev, NetBackend};
use rivemu_core::{
    GuestAddr, HostTimebase, IcountTimebase, Timebase, align_down, align_up, size::MIB,
};
use rivemu_cpu_riscv::csr::envcfg;
use rivemu_cpu_riscv::{Hart, HartIrqs, Logger};
use rivemu_devices::{Clint, GoldfishRtc, Plic, PowerControl, PowerEvent, SifiveTest, Uart16550};
use rivemu_mem::{AddressSpace, MapError, Ram};
use rivemu_virtio::{
    DmaRam, VirtioBlk, VirtioConsole, VirtioDevice, VirtioMmio, VirtioNet, VirtioRng,
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
        })
    } else {
        let (start, end) = loader::load_raw(bus, raw_addr, &data).map_err(wrap)?;
        Ok(ImageInfo {
            entry: start,
            start,
            end,
            elf: false,
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
                let data = read_file("initrd", initrd_path)?;
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
        for h in &mut self.harts {
            let ran = h.run(&self.bus, QUANTUM);
            if let Some(ic) = &self.icount {
                ic.advance(ran);
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

    /// Whether all harts are waiting for an interrupt.
    pub fn all_idle(&self) -> bool {
        self.harts.iter().all(|h| h.wfi)
    }

    fn now_ticks(&self) -> u64 {
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
            .filter(|h| h.csr.menvcfg & envcfg::STCE != 0 && h.csr.stimecmp > now)
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

    /// Attaches a diagnostic logger (`-d` / `-D`) to every hart.
    pub fn set_logger(&mut self, logger: Option<Arc<Logger>>) {
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
}
