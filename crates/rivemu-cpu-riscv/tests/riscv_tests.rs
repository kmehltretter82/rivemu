//! Runs the official riscv-tests ISA suite (built by
//! `scripts/build-riscv-tests.sh` into `images/riscv-tests/build`).
//!
//! Skipped when the binaries are missing, unless `RIVEMU_REQUIRE_RISCV_TESTS`
//! is set (CI). Filter with `RIVEMU_RISCV_TESTS_FILTER=rv64ui-p`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rivemu_core::{AccessSize, FrozenTimebase, size::MIB};
use rivemu_cpu_riscv::Hart;
use rivemu_machine::loader;
use rivemu_mem::{AddressSpace, Ram};

const MAX_INSNS: u64 = 20_000_000;

fn build_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../images/riscv-tests/build")
}

/// Runs one test binary; returns `Err(reason)` on failure.
fn run_test(path: &Path) -> Result<u64, String> {
    let data = std::fs::read(path).map_err(|e| e.to_string())?;
    let mut bus = AddressSpace::new();
    bus.add_ram("ram", 0x8000_0000, Arc::new(Ram::new(64 * MIB).unwrap()))
        .unwrap();
    let elf = loader::load_elf(&bus, &data).map_err(|e| e.to_string())?;
    let tohost = *elf.symbols.get("tohost").ok_or("no tohost symbol")?;

    let mut hart = Hart::new(0, Arc::new(FrozenTimebase(0)));
    hart.reset(elf.entry);
    let mut executed = 0;
    while executed < MAX_INSNS {
        executed += hart.run(&bus, 1000).max(1);
        let v = bus.read(tohost, AccessSize::Double).unwrap();
        if v == 0 {
            continue;
        }
        if v >> 48 == 0x0101 {
            // Character output from the "v" environment.
            bus.write(tohost, AccessSize::Double, 0).unwrap();
            continue;
        }
        return if v == 1 {
            Ok(executed)
        } else {
            Err(format!("failed test case #{} (pc={:#x})", v >> 1, hart.pc))
        };
    }
    Err(format!(
        "timeout after {MAX_INSNS} instructions (pc={:#x})",
        hart.pc
    ))
}

#[test]
fn riscv_tests() {
    let dir = build_dir();
    let required = std::env::var_os("RIVEMU_REQUIRE_RISCV_TESTS").is_some();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        assert!(!required, "riscv-tests not built at {}", dir.display());
        eprintln!("skipping: riscv-tests not built (run scripts/build-riscv-tests.sh)");
        return;
    };
    let filter = std::env::var("RIVEMU_RISCV_TESTS_FILTER").unwrap_or_default();
    let mut tests: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            let name = p.file_name().unwrap().to_string_lossy();
            !name.starts_with('.') && name.contains(filter.as_str())
        })
        .collect();
    tests.sort();
    assert!(
        !tests.is_empty(),
        "no riscv-tests found in {}",
        dir.display()
    );

    let mut failures = Vec::new();
    for t in &tests {
        let name = t.file_name().unwrap().to_string_lossy().into_owned();
        match run_test(t) {
            Ok(_) => {}
            Err(e) => failures.push(format!("{name}: {e}")),
        }
    }
    eprintln!(
        "riscv-tests: {}/{} passed",
        tests.len() - failures.len(),
        tests.len()
    );
    assert!(
        failures.is_empty(),
        "failures:\n  {}",
        failures.join("\n  ")
    );
}

#[test]
fn trap_vector_csrs_retain_value_for_reserved_modes() {
    use rivemu_cpu_riscv::csr;

    let mut h = Hart::new(0, Arc::new(FrozenTimebase(0)));

    for csr_num in [csr::MTVEC, csr::STVEC] {
        let direct = 0x8000_0100;
        assert_eq!(h.csr_write(csr_num, direct), Some(()));
        assert_eq!(h.csr_read(csr_num), Some(direct));

        let vectored = 0x8000_0201;
        assert_eq!(h.csr_write(csr_num, vectored), Some(()));
        assert_eq!(h.csr_read(csr_num), Some(vectored));

        for reserved in [0x9000_0302, 0xa000_0403] {
            assert_eq!(h.csr_write(csr_num, reserved), Some(()));
            assert_eq!(
                h.csr_read(csr_num),
                Some(vectored),
                "reserved MODE must retain the complete prior CSR value"
            );
        }
    }
}

#[test]
fn strict_align_traps_on_misaligned_loads_and_stores() {
    use rivemu_cpu_riscv::cause;

    let mut bus = AddressSpace::new();
    bus.add_ram("ram", 0x8000_0000, Arc::new(Ram::new(MIB).unwrap()))
        .unwrap();
    // 0x8000_0000: lw x1, 2(x10)   (0x0025_2083)
    // 0x8000_0004: sd x1, 4(x10)   (0x0015_3223)
    bus.write(0x8000_0000, AccessSize::Word, 0x0025_2083)
        .unwrap();
    bus.write(0x8000_0004, AccessSize::Word, 0x0015_3223)
        .unwrap();

    let mut h = Hart::new(0, Arc::new(FrozenTimebase(0)));
    h.strict_align = true;
    h.reset(0x8000_0000);
    h.csr.mtvec = 0x8000_0100;
    h.x[10] = 0x8000_1000;

    h.step(&bus);
    assert_eq!(h.pc, 0x8000_0100);
    assert_eq!(h.csr.mcause, cause::LOAD_MISALIGNED);
    assert_eq!(h.csr.mtval, 0x8000_1002);

    h.pc = 0x8000_0004;
    h.step(&bus);
    assert_eq!(h.pc, 0x8000_0100);
    assert_eq!(h.csr.mcause, cause::STORE_MISALIGNED);
    assert_eq!(h.csr.mtval, 0x8000_1004);
}

#[test]
fn failed_sc_checks_target_access_before_retiring() {
    use rivemu_cpu_riscv::cause;

    let mut bus = AddressSpace::new();
    bus.add_ram("ram", 0x8000_0000, Arc::new(Ram::new(MIB).unwrap()))
        .unwrap();
    bus.add_rom("rom", 0x1000, Arc::new(Ram::new(0x100).unwrap()))
        .unwrap();

    // sc.w x5, x6, (x10), repeated for three independent target cases.
    for pc in [0x8000_0000, 0x8000_0004, 0x8000_0008] {
        bus.write(pc, AccessSize::Word, 0x1865_22af).unwrap();
    }

    let trap_pc = 0x8000_0100;
    let mut h = Hart::new(0, Arc::new(FrozenTimebase(0)));
    h.reset(0x8000_0000);
    h.csr.mtvec = trap_pc;
    h.x[6] = 0xdead_beef;

    h.x[5] = 0x1111;
    h.x[10] = 0x4000_0000;
    h.step(&bus);
    assert_eq!(h.pc, trap_pc);
    assert_eq!(h.csr.mcause, cause::STORE_ACCESS);
    assert_eq!(h.csr.mtval, 0x4000_0000);
    assert_eq!(h.x[5], 0x1111, "a trapping SC must not write rd");

    h.pc = 0x8000_0004;
    h.x[5] = 0x2222;
    h.x[10] = 0x1000;
    h.step(&bus);
    assert_eq!(h.pc, trap_pc);
    assert_eq!(h.csr.mcause, cause::STORE_ACCESS);
    assert_eq!(h.csr.mtval, 0x1000);
    assert_eq!(h.x[5], 0x2222, "a trapping SC must not write rd");

    let target = 0x8000_1000;
    bus.write(target, AccessSize::Word, 0x5566_7788).unwrap();
    h.pc = 0x8000_0008;
    h.x[5] = 0x3333;
    h.x[10] = target;
    h.step(&bus);
    assert_eq!(h.pc, 0x8000_000c);
    assert_eq!(h.x[5], 1, "SC without a reservation must fail on valid RAM");
    assert_eq!(bus.read(target, AccessSize::Word).unwrap(), 0x5566_7788);
}

#[test]
fn svade_faults_on_missing_a_and_d_bits() {
    use rivemu_cpu_riscv::csr::envcfg;
    use rivemu_cpu_riscv::{Priv, cause};

    let mut bus = AddressSpace::new();
    bus.add_ram("ram", 0x8000_0000, Arc::new(Ram::new(MIB).unwrap()))
        .unwrap();

    // Sv39 1 GiB megapage leaf PTE at root index 2 (VA 0x8000_0000 -> PA 0x8000_0000):
    // PPN = 0x80000, V|R|W|X (0x0f), but A=0, D=0 initially.
    let root_pt = 0x8001_0000u64;
    let pte_addr = root_pt + 2 * 8;
    let pte_base = (0x80000u64 << 10) | 0x0f;
    bus.write(pte_addr, AccessSize::Double, pte_base).unwrap();

    // Code at 0x8000_0000:
    //   lw x1, 0(x10)  (0x0005_2083)
    //   sw x1, 0(x10)  (0x0015_2023)
    bus.write(0x8000_0000, AccessSize::Word, 0x0005_2083)
        .unwrap();
    bus.write(0x8000_0004, AccessSize::Word, 0x0015_2023)
        .unwrap();

    let mut h = Hart::new(0, Arc::new(FrozenTimebase(0)));
    h.svade = true;
    h.reset(0x8000_0000);
    h.prv = Priv::Supervisor;
    h.csr.satp = (8u64 << 60) | (root_pt >> 12);
    h.csr.medeleg = 0xffff;
    h.csr.stvec = 0x8000_0200;
    h.x[10] = 0x8000_2000;

    // 1. With A=0, instruction fetch at 0x8000_0000 raises INSN_PAGE_FAULT (12).
    h.step(&bus);
    assert_eq!(h.csr.scause, cause::INSN_PAGE_FAULT);
    assert_eq!(h.csr.stval, 0x8000_0000);
    assert_eq!(bus.read(pte_addr, AccessSize::Double).unwrap(), pte_base);

    // 2. OS sets A=1 (bit 6), leaving D=0 (bit 7). Fetch and load succeed, but store faults!
    bus.write(pte_addr, AccessSize::Double, pte_base | (1 << 6))
        .unwrap();
    h.invalidate_tlb();
    h.pc = 0x8000_0000;
    h.step(&bus);
    assert_eq!(h.pc, 0x8000_0004, "load with A=1 should succeed");
    h.step(&bus);
    assert_eq!(h.csr.scause, cause::STORE_PAGE_FAULT);
    assert_eq!(h.csr.stval, 0x8000_2000);

    // 3. Enabling menvcfg.ADUE allows hardware A/D updates even when h.svade is true.
    h.csr.menvcfg |= envcfg::ADUE;
    h.invalidate_tlb();
    h.pc = 0x8000_0004;
    h.step(&bus);
    assert_eq!(h.pc, 0x8000_0008, "store with ADUE=1 should succeed");
    assert_ne!(
        bus.read(pte_addr, AccessSize::Double).unwrap() & (1 << 7),
        0
    );
}

#[test]
fn pmp_enforces_napot_tor_na4_and_lock_bits() {
    use rivemu_cpu_riscv::csr;
    use rivemu_cpu_riscv::{Priv, cause};

    let mut bus = AddressSpace::new();
    bus.add_ram("ram", 0x8000_0000, Arc::new(Ram::new(MIB).unwrap()))
        .unwrap();
    // 0x8000_0000: lw x1, 0(x10)  (0x0005_2083)
    // 0x8000_0004: sw x1, 0(x10)  (0x0015_2023)
    // 0x8000_0008: ld x1, 0(x10)  (0x0005_3083)
    bus.write(0x8000_0000, AccessSize::Word, 0x0005_2083)
        .unwrap();
    bus.write(0x8000_0004, AccessSize::Word, 0x0015_2023)
        .unwrap();
    bus.write(0x8000_0008, AccessSize::Word, 0x0005_3083)
        .unwrap();

    let mut h = Hart::new(0, Arc::new(FrozenTimebase(0)));
    h.reset(0x8000_0000);
    h.csr.mtvec = 0x8000_0100;
    h.x[10] = 0x8000_2000;

    // Entry 0: NA4 (4-byte region) at 0x8000_2000 with R=1, W=0, X=0 (cfg = 0x11).
    // Entry 1: NAPOT catch-all covering all physical memory with R|W|X (cfg = 0x1f).
    h.csr_write(csr::PMPADDR0, 0x8000_2000 >> 2).unwrap();
    h.csr_write(csr::PMPADDR0 + 1, (1u64 << 54) - 1).unwrap();
    h.csr_write(csr::PMPCFG0, 0x11 | (0x1f << 8)).unwrap();

    // In S-mode: 4-byte load at 0x8000_2000 succeeds, 4-byte store faults, and
    // 8-byte load straddling the 4-byte NA4 boundary faults!
    h.prv = Priv::Supervisor;
    h.step(&bus);
    assert_eq!(h.pc, 0x8000_0004, "S-mode read of R=1 NA4 region succeeds");

    h.step(&bus);
    assert_eq!(h.csr.mcause, cause::STORE_ACCESS);
    assert_eq!(h.csr.mtval, 0x8000_2000);

    h.prv = Priv::Supervisor;
    h.pc = 0x8000_0008;
    h.step(&bus);
    assert_eq!(
        h.csr.mcause,
        cause::LOAD_ACCESS,
        "8-byte load straddling 4-byte NA4 PMP boundary must fault"
    );

    // Lock Entry 0 (L=1, NA4, R=1, W=0 -> 0x91): now even M-mode stores must fault,
    // and writes to PMPADDR0 / PMPCFG0[0] are ignored!
    h.csr_write(csr::PMPCFG0, 0x91 | (0x1f << 8)).unwrap();
    h.csr_write(csr::PMPADDR0, 0).unwrap();
    assert_eq!(h.csr.pmpaddr[0], 0x8000_2000 >> 2);

    h.prv = Priv::Machine;
    h.pc = 0x8000_0004;
    h.step(&bus);
    assert_eq!(
        h.csr.mcause,
        cause::STORE_ACCESS,
        "locked PMP entry must also fault in M-mode"
    );
}

#[test]
fn pmp_mxr_does_not_make_execute_only_region_readable() {
    use rivemu_cpu_riscv::csr;
    use rivemu_cpu_riscv::csr::status;
    use rivemu_cpu_riscv::{Priv, cause};

    let mut bus = AddressSpace::new();
    bus.add_ram("ram", 0x8000_0000, Arc::new(Ram::new(MIB).unwrap()))
        .unwrap();

    const CODE: u64 = 0x8000_0000;
    const TRAP: u64 = 0x8000_0100;
    const DATA: u64 = 0x8000_2000;
    bus.write(CODE, AccessSize::Word, 0x0005_2083) // lw x1, 0(x10)
        .unwrap();
    bus.write(DATA, AccessSize::Word, 0x1234_5678).unwrap();

    let mut h = Hart::new(0, Arc::new(FrozenTimebase(0)));
    h.reset(CODE);
    h.csr.mtvec = TRAP;
    h.x[10] = DATA;

    // Entry 0 is an exact NA4 execute-only data region. Entry 1 is a NAPOT
    // catch-all so S-mode instruction fetch remains permitted.
    h.csr_write(csr::PMPADDR0, DATA >> 2).unwrap();
    h.csr_write(csr::PMPADDR0 + 1, (1u64 << 54) - 1).unwrap();
    h.csr_write(csr::PMPCFG0, 0x14 | (0x1f << 8)).unwrap();

    h.csr.mstatus |= status::MXR;
    h.prv = Priv::Supervisor;
    h.step(&bus);

    assert_eq!(h.pc, TRAP);
    assert_eq!(h.csr.mcause, cause::LOAD_ACCESS);
    assert_eq!(h.csr.mtval, DATA);
}

#[test]
fn pmp_mxr_tlb_hit_does_not_bypass_execute_only_region() {
    use rivemu_cpu_riscv::csr;
    use rivemu_cpu_riscv::csr::status;
    use rivemu_cpu_riscv::{Priv, cause};

    let mut bus = AddressSpace::new();
    bus.add_ram("ram", 0x8000_0000, Arc::new(Ram::new(MIB).unwrap()))
        .unwrap();

    const CODE: u64 = 0x8000_0000;
    const TRAP: u64 = 0x8000_0100;
    const DATA: u64 = 0x8000_2000;
    const ROOT_PT: u64 = 0x8001_0000;
    bus.write(CODE, AccessSize::Word, 0x0005_2083) // lw x1, 0(x10)
        .unwrap();
    bus.write(DATA, AccessSize::Word, 0x1234_5678).unwrap();

    // Identity-map the low 1 GiB at 0x8000_0000 with an execute-only Sv39
    // megapage. MXR permits the load through this PTE, but not through PMP.
    let root_pte = (0x8000_0000 >> 2) | 0x49; // V|X|A
    bus.write(ROOT_PT + 2 * 8, AccessSize::Double, root_pte)
        .unwrap();

    let mut h = Hart::new(0, Arc::new(FrozenTimebase(0)));
    h.reset(CODE);
    h.csr.mtvec = TRAP;
    h.x[10] = DATA;
    h.csr_write(csr::SATP, (8 << 60) | (ROOT_PT >> 12)).unwrap();

    // Entry 0 is a page-aligned, execute-only NAPOT region covering DATA.
    // Entry 1 permits the code and page-table memory. This page alignment
    // reproduces the formerly vulnerable cached-translation path.
    h.csr_write(csr::PMPADDR0, (DATA >> 2) | 0x1ff).unwrap();
    h.csr_write(csr::PMPADDR0 + 1, (1u64 << 54) - 1).unwrap();
    h.csr_write(csr::PMPCFG0, 0x1c | (0x1f << 8)).unwrap();

    h.csr.mstatus |= status::MXR;
    h.prv = Priv::Supervisor;

    // The walk fills the TLB before the final PMP check rejects the load.
    h.step(&bus);
    assert_eq!(h.pc, TRAP);
    assert_eq!(h.csr.mcause, cause::LOAD_ACCESS);
    assert_eq!(h.csr.mtval, DATA);

    // Retrying the same instruction must still enforce the independent PMP
    // read permission when translation is served by the populated TLB entry.
    h.pc = CODE;
    h.prv = Priv::Supervisor;
    h.step(&bus);
    assert_eq!(h.pc, TRAP);
    assert_eq!(h.csr.mcause, cause::LOAD_ACCESS);
    assert_eq!(h.csr.mtval, DATA);
}

#[test]
fn lr_sc_64byte_cache_line_granularity_across_harts() {
    let mut bus = AddressSpace::new();
    bus.add_ram("ram", 0x8000_0000, Arc::new(Ram::new(MIB).unwrap()))
        .unwrap();

    // Code at 0x8000_0000:
    //   0x00: lr.d x1, (x10)       (0x1005_30af)
    //   0x04: sc.d x2, x11, (x10)  (0x18b5_312f)
    bus.write(0x8000_0000, AccessSize::Word, 0x1005_30af)
        .unwrap();
    bus.write(0x8000_0004, AccessSize::Word, 0x18b5_312f)
        .unwrap();

    let mut h0 = Hart::new(0, Arc::new(FrozenTimebase(0)));
    let mut h1 = Hart::new(1, Arc::new(FrozenTimebase(0)));
    h0.reset(0x8000_0000);
    h1.reset(0x8000_0000);

    // Both harts target DIFFERENT 8-byte words inside the SAME 64-byte cache line:
    // h0 at 0x8000_1000, h1 at 0x8000_1020 (+32 bytes).
    h0.x[10] = 0x8000_1000;
    h0.x[11] = 0xaaaa;
    h1.x[10] = 0x8000_1020;
    h1.x[11] = 0xbbbb;

    // Both harts execute lr.d on the same 64-byte cache line.
    h0.step(&bus);
    h1.step(&bus);

    // h0 executes sc.d first -> succeeds (x2 == 0).
    h0.step(&bus);
    assert_eq!(h0.x[2], 0);
    assert_eq!(bus.read(0x8000_1000, AccessSize::Double).unwrap(), 0xaaaa);

    // h1 executes sc.d second -> fails (x2 == 1) because h0's store to the same
    // 64-byte cache line invalidated h1's reservation!
    h1.step(&bus);
    assert_eq!(h1.x[2], 1);
    assert_eq!(bus.read(0x8000_1020, AccessSize::Double).unwrap(), 0);
}
