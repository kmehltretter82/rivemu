//! The RISC-V `virt` board.
//!
//! Memory map and image placement follow QEMU's `hw/riscv/virt.c` so that
//! firmware, kernels and device trees built for QEMU work unchanged.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rivemu_core::{GuestAddr, align_up, size::MIB};
use rivemu_mem::{AddressSpace, MapError, Ram};
use thiserror::Error;

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
/// Maximum number of harts supported by the board.
pub const MAX_HARTS: u32 = 512;

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

        Ok(Self {
            config,
            bus,
            ram,
            boot,
        })
    }
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
}
