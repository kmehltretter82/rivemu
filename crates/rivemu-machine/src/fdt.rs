//! Device tree generation for the `virt` board (QEMU-compatible layout).

use rivemu_fdt::FdtBuilder;

use crate::virt::{PCIE_IRQ, RTC_IRQ, TIMEBASE_FREQ, UART0_IRQ, memmap};

/// Inputs for device tree generation.
#[derive(Debug, Clone)]
pub(crate) struct FdtParams<'a> {
    pub(crate) harts: u32,
    pub(crate) ram_size: u64,
    pub(crate) bootargs: Option<&'a str>,
    pub(crate) initrd: Option<(u64, u64)>,
    pub(crate) plic_ndev: u32,
    /// (base, irq) of each virtio-mmio transport to describe.
    pub(crate) virtio: Vec<(u64, u32)>,
}

const ISA: &str = "rv64imafdc_zba_zbb_zbc_zbs_zicntr_zicsr_zifencei_zihintpause_sstc";
const ISA_EXTENSIONS: &[&str] = &[
    "i",
    "m",
    "a",
    "f",
    "d",
    "c",
    "zba",
    "zbb",
    "zbc",
    "zbs",
    "zicntr",
    "zicsr",
    "zifencei",
    "zihintpause",
    "sstc",
];

pub(crate) fn build(p: &FdtParams<'_>) -> Vec<u8> {
    let mut f = FdtBuilder::new();
    let intc: Vec<u32> = (0..p.harts).map(|_| f.alloc_phandle()).collect();
    let cpu_ph: Vec<u32> = (0..p.harts).map(|_| f.alloc_phandle()).collect();
    let plic_ph = f.alloc_phandle();
    let test_ph = f.alloc_phandle();

    f.begin_node("");
    f.prop_u32("#address-cells", 2);
    f.prop_u32("#size-cells", 2);
    f.prop_str("compatible", "riscv-virtio");
    f.prop_str("model", "riscv-virtio,qemu");

    f.begin_node("chosen");
    if let Some(args) = p.bootargs {
        f.prop_str("bootargs", args);
    }
    f.prop_str("stdout-path", &format!("/soc/serial@{:x}", memmap::UART0.0));
    if let Some((start, end)) = p.initrd {
        f.prop_u64("linux,initrd-start", start);
        f.prop_u64("linux,initrd-end", end);
    }
    f.end_node();

    f.begin_node(&format!("memory@{:x}", memmap::DRAM_BASE));
    f.prop_str("device_type", "memory");
    f.prop_u64s("reg", &[memmap::DRAM_BASE, p.ram_size]);
    f.end_node();

    f.begin_node("cpus");
    f.prop_u32("#address-cells", 1);
    f.prop_u32("#size-cells", 0);
    f.prop_u32("timebase-frequency", TIMEBASE_FREQ as u32);
    for h in 0..p.harts as usize {
        f.begin_node(&format!("cpu@{h}"));
        f.prop_u32("phandle", cpu_ph[h]);
        f.prop_str("device_type", "cpu");
        f.prop_u32("reg", h as u32);
        f.prop_str("status", "okay");
        f.prop_str("compatible", "riscv");
        f.prop_str("riscv,isa", ISA);
        f.prop_str("riscv,isa-base", "rv64i");
        f.prop_strs("riscv,isa-extensions", ISA_EXTENSIONS);
        f.prop_str("mmu-type", "riscv,sv39");
        f.begin_node("interrupt-controller");
        f.prop_u32("#interrupt-cells", 1);
        f.prop_null("interrupt-controller");
        f.prop_str("compatible", "riscv,cpu-intc");
        f.prop_u32("phandle", intc[h]);
        f.end_node();
        f.end_node();
    }
    f.begin_node("cpu-map");
    f.begin_node("cluster0");
    for (h, &ph) in cpu_ph.iter().enumerate() {
        f.begin_node(&format!("core{h}"));
        f.prop_u32("cpu", ph);
        f.end_node();
    }
    f.end_node();
    f.end_node();
    f.end_node(); // cpus

    f.begin_node("soc");
    f.prop_u32("#address-cells", 2);
    f.prop_u32("#size-cells", 2);
    f.prop_str("compatible", "simple-bus");
    f.prop_null("ranges");

    let reg = |f: &mut FdtBuilder, (base, size): (u64, u64)| f.prop_u64s("reg", &[base, size]);

    f.begin_node(&format!("clint@{:x}", memmap::CLINT.0));
    f.prop_strs("compatible", &["sifive,clint0", "riscv,clint0"]);
    reg(&mut f, memmap::CLINT);
    let cells: Vec<u32> = intc.iter().flat_map(|&i| [i, 3, i, 7]).collect();
    f.prop_cells("interrupts-extended", &cells);
    f.end_node();

    f.begin_node(&format!("plic@{:x}", memmap::PLIC.0));
    f.prop_u32("phandle", plic_ph);
    f.prop_strs("compatible", &["sifive,plic-1.0.0", "riscv,plic0"]);
    reg(&mut f, memmap::PLIC);
    f.prop_u32("#interrupt-cells", 1);
    f.prop_u32("#address-cells", 0);
    f.prop_null("interrupt-controller");
    f.prop_u32("riscv,ndev", p.plic_ndev);
    let cells: Vec<u32> = intc.iter().flat_map(|&i| [i, 11, i, 9]).collect();
    f.prop_cells("interrupts-extended", &cells);
    f.end_node();

    f.begin_node(&format!("serial@{:x}", memmap::UART0.0));
    f.prop_str("compatible", "ns16550a");
    reg(&mut f, memmap::UART0);
    f.prop_u32("clock-frequency", 0x38_4000);
    f.prop_u32("interrupts", UART0_IRQ);
    f.prop_u32("interrupt-parent", plic_ph);
    f.end_node();

    f.begin_node(&format!("rtc@{:x}", memmap::RTC.0));
    f.prop_str("compatible", "google,goldfish-rtc");
    reg(&mut f, memmap::RTC);
    f.prop_u32("interrupts", RTC_IRQ);
    f.prop_u32("interrupt-parent", plic_ph);
    f.end_node();

    f.begin_node(&format!("test@{:x}", memmap::TEST.0));
    f.prop_u32("phandle", test_ph);
    f.prop_strs("compatible", &["sifive,test1", "sifive,test0", "syscon"]);
    reg(&mut f, memmap::TEST);
    f.end_node();

    f.begin_node("poweroff");
    f.prop_str("compatible", "syscon-poweroff");
    f.prop_u32("regmap", test_ph);
    f.prop_u32("offset", 0);
    f.prop_u32("value", 0x5555);
    f.end_node();

    f.begin_node("reboot");
    f.prop_str("compatible", "syscon-reboot");
    f.prop_u32("regmap", test_ph);
    f.prop_u32("offset", 0);
    f.prop_u32("value", 0x7777);
    f.end_node();

    for &(base, irq) in &p.virtio {
        f.begin_node(&format!("virtio_mmio@{base:x}"));
        f.prop_str("compatible", "virtio,mmio");
        reg(&mut f, (base, memmap::VIRTIO.1));
        f.prop_u32("interrupts", irq);
        f.prop_u32("interrupt-parent", plic_ph);
        f.end_node();
    }

    f.begin_node(&format!("fw-cfg@{:x}", memmap::FW_CFG.0));
    f.prop_str("compatible", "qemu,fw-cfg-mmio");
    reg(&mut f, memmap::FW_CFG);
    f.prop_null("dma-coherent");
    f.end_node();

    f.begin_node(&format!("flash@{:x}", memmap::FLASH.0));
    f.prop_str("compatible", "cfi-flash");
    let bank_size = memmap::FLASH.1 / 2;
    f.prop_u64s(
        "reg",
        &[
            memmap::FLASH.0,
            bank_size,
            memmap::FLASH.0 + bank_size,
            bank_size,
        ],
    );
    f.prop_u32("bank-width", 4);
    f.end_node();

    f.begin_node(&format!("pci@{:x}", memmap::PCIE_ECAM.0));
    f.prop_str("compatible", "pci-host-ecam-generic");
    f.prop_str("device_type", "pci");
    f.prop_u32("#address-cells", 3);
    f.prop_u32("#size-cells", 2);
    f.prop_u32("#interrupt-cells", 1);
    f.prop_cells("bus-range", &[0x00, 0xff]);
    f.prop_null("dma-coherent");
    reg(&mut f, memmap::PCIE_ECAM);
    f.prop_cells(
        "ranges",
        &[
            // I/O space (0x01000000): 64 KiB at 0x0300_0000
            0x0100_0000,
            0,
            0,
            (memmap::PCIE_PIO.0 >> 32) as u32,
            memmap::PCIE_PIO.0 as u32,
            (memmap::PCIE_PIO.1 >> 32) as u32,
            memmap::PCIE_PIO.1 as u32,
            // 32-bit MMIO space (0x02000000): 1 GiB at 0x4000_0000
            0x0200_0000,
            (memmap::PCIE_MMIO.0 >> 32) as u32,
            memmap::PCIE_MMIO.0 as u32,
            (memmap::PCIE_MMIO.0 >> 32) as u32,
            memmap::PCIE_MMIO.0 as u32,
            (memmap::PCIE_MMIO.1 >> 32) as u32,
            memmap::PCIE_MMIO.1 as u32,
        ],
    );
    f.prop_cells("interrupt-map-mask", &[0x1800, 0, 0, 0x7]);
    let mut imap = Vec::with_capacity(16 * 6);
    for slot in 0..4u32 {
        for pin in 1..=4u32 {
            let irq = PCIE_IRQ + ((slot + pin - 1) % 4);
            imap.extend_from_slice(&[slot << 11, 0, 0, pin, plic_ph, irq]);
        }
    }
    f.prop_cells("interrupt-map", &imap);
    f.end_node();

    f.end_node(); // soc
    f.end_node(); // root
    f.finish(0)
}
