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

#[test]
fn zicbom_permits_read_only_targets_and_reports_store_fault_causes() {
    use rivemu_cpu_riscv::csr::{self, envcfg, status};
    use rivemu_cpu_riscv::{Priv, cause};

    let mut bus = AddressSpace::new();
    bus.add_ram("ram", 0x8000_0000, Arc::new(Ram::new(MIB).unwrap()))
        .unwrap();

    const CODE: u64 = 0x8000_0000;
    const TRAP: u64 = 0x8000_0100;
    const DATA: u64 = 0x8000_2000;
    const ROOT_PT: u64 = 0x8001_0000;
    const L1_PT: u64 = 0x8001_1000;
    const L0_PT: u64 = 0x8001_2000;

    // 0x8000_0000: cbo.inval (x10)  (0x0005_200f)
    // 0x8000_0004: cbo.clean (x10)  (0x0015_200f)
    // 0x8000_0008: cbo.flush (x10)  (0x0025_200f)
    bus.write(CODE, AccessSize::Word, 0x0005_200f).unwrap();
    bus.write(CODE + 4, AccessSize::Word, 0x0015_200f).unwrap();
    bus.write(CODE + 8, AccessSize::Word, 0x0025_200f).unwrap();

    // 3-level Sv39 page table:
    // - CODE (0x8000_0000) is V|R|X|A (0x4b).
    // - DATA (0x8000_2000) starts as read-only V|R|A (0x43, W=0, D=0).
    bus.write(ROOT_PT + 2 * 8, AccessSize::Double, ((L1_PT >> 12) << 10) | 1)
        .unwrap();
    bus.write(L1_PT, AccessSize::Double, ((L0_PT >> 12) << 10) | 1)
        .unwrap();
    bus.write(L0_PT, AccessSize::Double, ((CODE >> 12) << 10) | 0x4b)
        .unwrap();
    let data_pte_addr = L0_PT + 2 * 8;
    let ro_data_pte = ((DATA >> 12) << 10) | 0x43;
    let xo_data_pte = ((DATA >> 12) << 10) | 0x49;
    bus.write(data_pte_addr, AccessSize::Double, ro_data_pte)
        .unwrap();

    let mut h = Hart::new(0, Arc::new(FrozenTimebase(0)));
    h.svade = true;
    h.reset(CODE);
    h.csr.mtvec = TRAP;
    h.csr.menvcfg = envcfg::CBIE_MASK | envcfg::CBCFE;
    h.csr_write(csr::SATP, (8 << 60) | (ROOT_PT >> 12)).unwrap();
    h.x[10] = DATA + 19;

    // 1. Read-only Sv39 page (R=1, W=0, A=1, D=0) with read-only PMP entry
    // permits cbo.inval, cbo.clean, and cbo.flush in S-mode.
    h.csr_write(csr::PMPADDR0, (DATA >> 2) | 0x1ff).unwrap();
    h.csr_write(csr::PMPADDR0 + 1, (1u64 << 54) - 1).unwrap();
    h.csr_write(csr::PMPCFG0, 0x19 | (0x1f << 8)).unwrap(); // Entry 0: NAPOT R=1, W=0, X=0
    h.prv = Priv::Supervisor;
    for expected_next_pc in [CODE + 4, CODE + 8, CODE + 12] {
        h.step(&bus);
        assert_eq!(h.pc, expected_next_pc);
    }

    // 2. When the page is read-only (R=1, W=0) in Sv39 and PMP denies both
    // read and write (execute-only PMP: R=0, W=0, X=1), all three instructions
    // must report STORE_ACCESS (7), not STORE_PAGE_FAULT (15) or LOAD_ACCESS (5).
    h.csr_write(csr::PMPCFG0, 0x1c | (0x1f << 8)).unwrap(); // Entry 0: NAPOT R=0, W=0, X=1
    for insn_pc in [CODE, CODE + 4, CODE + 8] {
        h.pc = insn_pc;
        h.prv = Priv::Supervisor;
        h.step(&bus);
        assert_eq!(h.pc, TRAP);
        assert_eq!(h.csr.mcause, cause::STORE_ACCESS);
        assert_eq!(h.csr.mtval, DATA + 19);
    }

    // 3. On an execute-only Sv39 page (R=0, W=0, X=1) with open PMP, MXR=0
    // faults with STORE_PAGE_FAULT (15), while MXR=1 permits all three.
    h.csr_write(csr::PMPCFG0, 0x1f | (0x1f << 8)).unwrap();
    bus.write(data_pte_addr, AccessSize::Double, xo_data_pte)
        .unwrap();
    h.invalidate_tlb();
    for insn_pc in [CODE, CODE + 4, CODE + 8] {
        h.pc = insn_pc;
        h.prv = Priv::Supervisor;
        h.step(&bus);
        assert_eq!(h.pc, TRAP);
        assert_eq!(h.csr.mcause, cause::STORE_PAGE_FAULT);
        assert_eq!(h.csr.mtval, DATA + 19);
    }

    h.csr.mstatus |= status::MXR;
    h.pc = CODE;
    h.prv = Priv::Supervisor;
    for expected_next_pc in [CODE + 4, CODE + 8, CODE + 12] {
        h.step(&bus);
        assert_eq!(h.pc, expected_next_pc);
    }
}

#[test]
fn sfence_vma_respects_asid_global_and_superpages() {
    use rivemu_cpu_riscv::{Priv, csr};

    let mut bus = AddressSpace::new();
    bus.add_ram("ram", 0x8000_0000, Arc::new(Ram::new(4 * MIB).unwrap()))
        .unwrap();

    const CODE: u64 = 0x8000_0000;
    const VA_LOCAL: u64 = 0x8000_2000;
    const VA_GLOBAL: u64 = 0x8000_3000;
    const VA_NONLEAF_G: u64 = 0x8020_0000;
    const VA_SUPER_A: u64 = 0x8040_1000;
    const VA_SUPER_B: u64 = 0x8040_5000;

    const PA_LOCAL_1: u64 = 0x8000_4000;
    const PA_LOCAL_2: u64 = 0x8000_5000;
    const PA_GLOBAL_1: u64 = 0x8000_6000;
    const PA_GLOBAL_2: u64 = 0x8000_7000;
    const PA_NONLEAF_1: u64 = 0x8000_8000;
    const PA_NONLEAF_2: u64 = 0x8000_9000;

    const ROOT_PT1: u64 = 0x8001_0000;
    const L1_PT1: u64 = 0x8001_1000;
    const L0_PT1: u64 = 0x8001_2000;
    const L0_G_PT1: u64 = 0x8001_3000;

    bus.write(PA_LOCAL_1, AccessSize::Word, 0x1111_1111).unwrap();
    bus.write(PA_LOCAL_2, AccessSize::Word, 0x2222_2222).unwrap();
    bus.write(PA_GLOBAL_1, AccessSize::Word, 0xaaaa_1111).unwrap();
    bus.write(PA_GLOBAL_2, AccessSize::Word, 0xaaaa_2222).unwrap();
    bus.write(PA_NONLEAF_1, AccessSize::Word, 0xbbbb_1111).unwrap();
    bus.write(PA_NONLEAF_2, AccessSize::Word, 0xbbbb_2222).unwrap();

    // Instructions at CODE:
    //   0x00: lwu x1, 0(x10)          (0x0005_6083)
    //   0x04: sfence.vma x0, x11      (0x12b0_0073) - ASID flush
    //   0x08: sfence.vma x10, x11     (0x12b5_0073) - VA + ASID flush
    //   0x0c: sfence.vma x10, x0      (0x1205_0073) - VA all-ASID (including global) flush
    //   0x10: sfence.vma x0, x0       (0x1200_0073) - Full flush
    bus.write(CODE, AccessSize::Word, 0x0005_6083).unwrap();
    bus.write(CODE + 4, AccessSize::Word, 0x12b0_0073).unwrap();
    bus.write(CODE + 8, AccessSize::Word, 0x12b5_0073).unwrap();
    bus.write(CODE + 12, AccessSize::Word, 0x1205_0073).unwrap();
    bus.write(CODE + 16, AccessSize::Word, 0x1200_0073).unwrap();

    // Build Sv39 page table:
    // Root[2] -> L1_PT1
    // L1_PT1[0] -> L0_PT1 (non-global branch)
    // L1_PT1[1] -> L0_G_PT1 with non-leaf G=1 (0x21)
    // L1_PT1[2] -> 2 MiB superpage at 0x8020_0000 (initially mapped to 0x8020_0000)
    bus.write(ROOT_PT1 + 2 * 8, AccessSize::Double, ((L1_PT1 >> 12) << 10) | 0x01)
        .unwrap();
    bus.write(L1_PT1, AccessSize::Double, ((L0_PT1 >> 12) << 10) | 0x01)
        .unwrap();
    bus.write(
        L1_PT1 + 8,
        AccessSize::Double,
        ((L0_G_PT1 >> 12) << 10) | 0x21, // V | G on non-leaf PTE!
    )
    .unwrap();
    bus.write(
        L1_PT1 + 2 * 8,
        AccessSize::Double,
        ((0x8020_0000u64 >> 12) << 10) | 0xcf, // 2 MiB leaf V|R|W|X|A|D
    )
    .unwrap();

    // L0_PT1[0]: CODE -> CODE (V|R|X|G|A = 0x6b)
    // L0_PT1[2]: VA_LOCAL -> PA_LOCAL_1 (V|R|A = 0x43, G=0)
    // L0_PT1[3]: VA_GLOBAL -> PA_GLOBAL_1 (V|R|G|A = 0x63, G=1)
    bus.write(L0_PT1, AccessSize::Double, ((CODE >> 12) << 10) | 0x6b)
        .unwrap();
    bus.write(
        L0_PT1 + 2 * 8,
        AccessSize::Double,
        ((PA_LOCAL_1 >> 12) << 10) | 0x43,
    )
    .unwrap();
    bus.write(
        L0_PT1 + 3 * 8,
        AccessSize::Double,
        ((PA_GLOBAL_1 >> 12) << 10) | 0x63,
    )
    .unwrap();
    // L0_G_PT1[0]: VA_NONLEAF_G -> PA_NONLEAF_1 with leaf G=0 (0x43), inherits G=1 from L1_PT1[1]!
    bus.write(
        L0_G_PT1,
        AccessSize::Double,
        ((PA_NONLEAF_1 >> 12) << 10) | 0x43,
    )
    .unwrap();

    let mut h = Hart::new(0, Arc::new(FrozenTimebase(0)));
    h.reset(CODE);
    h.prv = Priv::Supervisor;
    let satp_asid1 = (8u64 << 60) | (1u64 << 44) | (ROOT_PT1 >> 12);
    let satp_asid2 = (8u64 << 60) | (2u64 << 44) | (ROOT_PT1 >> 12);
    h.csr_write(csr::SATP, satp_asid1).unwrap();

    // 1. Warm TLB under ASID 1 for VA_LOCAL, VA_GLOBAL, and VA_NONLEAF_G.
    for (va, expected) in [
        (VA_LOCAL, 0x1111_1111),
        (VA_GLOBAL, 0xaaaa_1111),
        (VA_NONLEAF_G, 0xbbbb_1111),
    ] {
        h.pc = CODE;
        h.x[10] = va;
        h.step(&bus);
        assert_eq!(h.x[1], expected);
    }

    // Rewrite PTEs in RAM to point to the *_2 physical pages without sfence.vma.
    bus.write(
        L0_PT1 + 2 * 8,
        AccessSize::Double,
        ((PA_LOCAL_2 >> 12) << 10) | 0x43,
    )
    .unwrap();
    bus.write(
        L0_PT1 + 3 * 8,
        AccessSize::Double,
        ((PA_GLOBAL_2 >> 12) << 10) | 0x63,
    )
    .unwrap();
    bus.write(
        L0_G_PT1,
        AccessSize::Double,
        ((PA_NONLEAF_2 >> 12) << 10) | 0x43,
    )
    .unwrap();

    // 2. Switch to ASID 2: VA_GLOBAL (leaf G=1) and VA_NONLEAF_G (non-leaf G=1)
    // still hit the cached *_1 entries across the ASID change!
    h.csr_write(csr::SATP, satp_asid2).unwrap();
    for (va, expected) in [(VA_GLOBAL, 0xaaaa_1111), (VA_NONLEAF_G, 0xbbbb_1111)] {
        h.pc = CODE;
        h.x[10] = va;
        h.step(&bus);
        assert_eq!(h.x[1], expected);
    }

    // 3. Switch back to ASID 1 and execute `sfence.vma x0, x11` (ASID 2) and
    // `sfence.vma x10, x11` (VA_LOCAL, ASID 2): ASID 1's cached VA_LOCAL
    // (0x1111_1111) and global entries must survive!
    h.csr_write(csr::SATP, satp_asid1).unwrap();
    h.pc = CODE + 4;
    h.x[11] = 2;
    h.step(&bus);
    h.pc = CODE + 8;
    h.x[10] = VA_LOCAL;
    h.x[11] = 2;
    h.step(&bus);
    h.pc = CODE;
    h.x[10] = VA_LOCAL;
    h.step(&bus);
    assert_eq!(h.x[1], 0x1111_1111);

    // Executing `sfence.vma` under ASID 2 for VA_LOCAL now misses ASID 1's entry
    // and walks PA_LOCAL_2 (0x2222_2222).
    h.csr_write(csr::SATP, satp_asid2).unwrap();
    h.pc = CODE;
    h.x[10] = VA_LOCAL;
    h.step(&bus);
    assert_eq!(h.x[1], 0x2222_2222);

    // Re-warm VA_LOCAL with PA_LOCAL_1 under ASID 1, then rewrite PTE to PA_LOCAL_2.
    bus.write(
        L0_PT1 + 2 * 8,
        AccessSize::Double,
        ((PA_LOCAL_1 >> 12) << 10) | 0x43,
    )
    .unwrap();
    h.csr_write(csr::SATP, satp_asid1).unwrap();
    h.pc = CODE + 8;
    h.x[10] = VA_LOCAL;
    h.x[11] = 1;
    h.step(&bus);
    h.pc = CODE;
    h.x[10] = VA_LOCAL;
    h.step(&bus);
    assert_eq!(h.x[1], 0x1111_1111);
    bus.write(
        L0_PT1 + 2 * 8,
        AccessSize::Double,
        ((PA_LOCAL_2 >> 12) << 10) | 0x43,
    )
    .unwrap();

    // 4. Flush ASID 1 (`sfence.vma x0, x11` with x11=1):
    // VA_LOCAL (G=0) is invalidated and reloads 0x2222_2222, while both global
    // mappings (leaf G=1 and non-leaf G=1) survive ASID-scoped invalidation!
    h.pc = CODE + 4;
    h.x[11] = 1;
    h.step(&bus);
    for (va, expected) in [
        (VA_LOCAL, 0x2222_2222),
        (VA_GLOBAL, 0xaaaa_1111),
        (VA_NONLEAF_G, 0xbbbb_1111),
    ] {
        h.pc = CODE;
        h.x[10] = va;
        h.step(&bus);
        assert_eq!(h.x[1], expected);
    }

    // 5. `sfence.vma x10, x0` (rs2 = x0) invalidates global mappings at x10.
    for (va, expected) in [(VA_GLOBAL, 0xaaaa_2222), (VA_NONLEAF_G, 0xbbbb_2222)] {
        h.pc = CODE + 12;
        h.x[10] = va;
        h.step(&bus);
        h.pc = CODE;
        h.x[10] = va;
        h.step(&bus);
        assert_eq!(h.x[1], expected);
    }

    // 6. Superpage invalidation: map 2 MiB at VA 0x8040_0000 -> PA 0x8020_0000,
    // load from VA_SUPER_A (0x8040_1000), remap L1_PT1[2] -> PA 0x8000_0000, and
    // execute `sfence.vma x10, x0` with x10 = VA_SUPER_B (0x8040_5000, a different
    // 4 KiB page inside the same 2 MiB superpage).
    bus.write(0x8020_1000, AccessSize::Word, 0x5555_1111).unwrap();
    bus.write(0x8000_1000, AccessSize::Word, 0x5555_2222).unwrap();
    h.pc = CODE;
    h.x[10] = VA_SUPER_A;
    h.step(&bus);
    assert_eq!(h.x[1], 0x5555_1111);

    bus.write(
        L1_PT1 + 2 * 8,
        AccessSize::Double,
        ((0x8000_0000u64 >> 12) << 10) | 0xcf,
    )
    .unwrap();
    h.pc = CODE + 12;
    h.x[10] = VA_SUPER_B;
    h.step(&bus);

    h.pc = CODE;
    h.x[10] = VA_SUPER_A;
    h.step(&bus);
    assert_eq!(h.x[1], 0x5555_2222);
}

#[test]
fn mret_with_zero_pmp_rules_retires_before_fetch_access_fault() {
    use rivemu_cpu_riscv::{Priv, csr};

    let mut bus = AddressSpace::new();
    bus.add_ram("ram", 0x8000_0000, Arc::new(Ram::new(MIB).unwrap()))
        .unwrap();

    const MRET_PC: u64 = 0x8000_0000;
    const TARGET_PC: u64 = 0x8000_1000;
    const MTVEC_PC: u64 = 0x8000_2000;

    let mut h = Hart::new(0, Arc::new(FrozenTimebase(0)));
    h.strict_pmp = true;
    h.reset(MRET_PC);

    // MRET at MRET_PC, NOP at TARGET_PC
    bus.write(MRET_PC, AccessSize::Word, 0x3020_0073).unwrap();
    bus.write(TARGET_PC, AccessSize::Word, 0x0000_0013).unwrap();

    // All pmpcfg registers are 0 (0 active PMP rules).
    // Configure mstatus.MPP = S (1), mstatus.MPIE = 1, mstatus.MIE = 0, mepc = TARGET_PC, mtvec = MTVEC_PC.
    h.csr_write(csr::MTVEC, MTVEC_PC).unwrap();
    h.csr_write(csr::MEPC, TARGET_PC).unwrap();
    let mstatus = h.csr_read(csr::MSTATUS).unwrap();
    let mstatus = (mstatus & !((3 << 11) | (1 << 7) | (1 << 3))) | (1 << 11) | (1 << 7);
    h.csr_write(csr::MSTATUS, mstatus).unwrap();

    // Step 1: MRET retires cleanly, transitioning to S-mode at TARGET_PC with MIE=1, MPIE=1, MPP=U(0).
    h.step(&bus);
    assert_eq!(h.prv, Priv::Supervisor);
    assert_eq!(h.pc, TARGET_PC);
    let mid_mstatus = h.csr_read(csr::MSTATUS).unwrap();
    assert_eq!((mid_mstatus >> 11) & 3, 0);
    assert_eq!((mid_mstatus >> 7) & 1, 1);
    assert_eq!((mid_mstatus >> 3) & 1, 1);

    // Step 2: Subsequent instruction fetch at TARGET_PC in S-mode with 0 PMP rules raises
    // Instruction Access Fault (mcause = 1) with mepc = TARGET_PC, mtval = TARGET_PC,
    // mstatus.MPP = S (1), and mstatus.MPIE = 1.
    h.step(&bus);
    assert_eq!(h.prv, Priv::Machine);
    assert_eq!(h.pc, MTVEC_PC);
    assert_eq!(h.csr_read(csr::MCAUSE).unwrap(), 1);
    assert_eq!(h.csr_read(csr::MEPC).unwrap(), TARGET_PC);
    assert_eq!(h.csr_read(csr::MTVAL).unwrap(), TARGET_PC);
    let post_mstatus = h.csr_read(csr::MSTATUS).unwrap();
    assert_eq!((post_mstatus >> 11) & 3, 1);
    assert_eq!((post_mstatus >> 7) & 1, 1);
}

#[test]
fn sv48_four_level_walk_superpages_and_canonical_check() {
    use rivemu_cpu_riscv::{Priv, csr};

    let mut bus = AddressSpace::new();
    bus.add_ram("ram", 0x8000_0000, Arc::new(Ram::new(16 * MIB).unwrap()))
        .unwrap();

    const CODE: u64 = 0x8000_0000;
    const STVEC: u64 = 0x8000_1000;
    const PA_POS: u64 = 0x8000_2000;
    const PA_NEG: u64 = 0x8000_3000;
    const L3_PT: u64 = 0x8001_0000;
    const L2_POS_PT: u64 = 0x8001_1000;
    const L1_POS_PT: u64 = 0x8001_2000;
    const L0_POS_PT: u64 = 0x8001_3000;
    const L2_NEG_PT: u64 = 0x8001_4000;

    // Positive Sv48 4-level VA: vpn[3]=106 (0x6a), vpn[2]=170 (0xaa), vpn[1]=0, vpn[0]=2
    const VA_POS_4K: u64 = (106u64 << 39) | (170u64 << 30) | (2u64 << 12);
    // Negative canonical Sv48 VA (bits 63:47 all 1): vpn[3]=341 (0x155), vpn[2]=2 (1 GiB leaf)
    const VA_NEG_1G: u64 = 0xffff_8000_0000_0000u64 | (341u64 << 39) | (2u64 << 30) | 0x3000;
    // Misaligned 512 GiB L3 superpage at vpn[3]=200
    const VA_MISALIGNED_512G: u64 = 200u64 << 39;
    // Non-canonical Sv48 VAs: bit 47 = 1 with bits 63:48 = 0, and bit 47 = 0 with bits 63:48 = 0xffff
    const VA_NONCANON_POS: u64 = 1u64 << 47;
    const VA_NONCANON_NEG: u64 = 0xffff_0000_0000_0000u64;

    // lw x1, 0(x10)
    bus.write(CODE, AccessSize::Word, 0x0005_2083).unwrap();
    bus.write(PA_POS, AccessSize::Word, 0x1234_5678).unwrap();
    bus.write(PA_NEG, AccessSize::Word, 0x7654_3210).unwrap();

    // Identity-map 0x8000_0000 via L3_PT[0] -> L2_POS_PT[2] (1 GiB leaf V|R|W|X|A|D = 0xcf)
    bus.write(L3_PT, AccessSize::Double, ((L2_POS_PT >> 12) << 10) | 0x01)
        .unwrap();
    bus.write(
        L2_POS_PT + 2 * 8,
        AccessSize::Double,
        ((0x8000_0000u64 >> 12) << 10) | 0xcf,
    )
    .unwrap();

    // 4-level walk for VA_POS_4K: L3_PT[106] -> L2_POS_PT[170] -> L1_POS_PT[0] -> L0_POS_PT[2] -> PA_POS
    bus.write(
        L3_PT + 106 * 8,
        AccessSize::Double,
        ((L2_POS_PT >> 12) << 10) | 0x01,
    )
    .unwrap();
    bus.write(
        L2_POS_PT + 170 * 8,
        AccessSize::Double,
        ((L1_POS_PT >> 12) << 10) | 0x01,
    )
    .unwrap();
    bus.write(L1_POS_PT, AccessSize::Double, ((L0_POS_PT >> 12) << 10) | 0x01)
        .unwrap();
    bus.write(
        L0_POS_PT + 2 * 8,
        AccessSize::Double,
        ((PA_POS >> 12) << 10) | 0xc7,
    )
    .unwrap();

    // Negative canonical 1 GiB superpage walk: L3_PT[341] -> L2_NEG_PT[2] -> 0x8000_0000 (1 GiB leaf)
    bus.write(
        L3_PT + 341 * 8,
        AccessSize::Double,
        ((L2_NEG_PT >> 12) << 10) | 0x01,
    )
    .unwrap();
    bus.write(
        L2_NEG_PT + 2 * 8,
        AccessSize::Double,
        ((0x8000_0000u64 >> 12) << 10) | 0xc7,
    )
    .unwrap();

    // Misaligned 512 GiB L3 leaf at L3_PT[200]: PPN = 0x80000 (low 27 bits of PPN are non-zero!)
    bus.write(
        L3_PT + 200 * 8,
        AccessSize::Double,
        ((0x8000_0000u64 >> 12) << 10) | 0xc7,
    )
    .unwrap();

    let mut h = Hart::new(0, Arc::new(FrozenTimebase(0)));
    h.reset(CODE);
    h.prv = Priv::Supervisor;
    h.csr_write(csr::MEDELEG, 1 << 13).unwrap();
    h.csr_write(csr::STVEC, STVEC).unwrap();
    // satp.MODE = 9 (Sv48)
    h.csr_write(csr::SATP, (9u64 << 60) | (L3_PT >> 12))
        .unwrap();

    // 1. 4-level positive Sv48 walk and debug_translate
    assert_eq!(h.debug_translate(&bus, VA_POS_4K), Some(PA_POS));
    h.pc = CODE;
    h.x[10] = VA_POS_4K;
    h.step(&bus);
    assert_eq!(h.x[1], 0x1234_5678);

    // 2. Negative canonical Sv48 1 GiB superpage walk and debug_translate
    assert_eq!(h.debug_translate(&bus, VA_NEG_1G), Some(PA_NEG));
    h.pc = CODE;
    h.x[10] = VA_NEG_1G;
    h.step(&bus);
    assert_eq!(h.x[1], 0x7654_3210);

    // 3. Misaligned 512 GiB L3 superpage and non-canonical Sv48 VAs must fault with Load Page Fault (13)
    for bad_va in [VA_MISALIGNED_512G, VA_NONCANON_POS, VA_NONCANON_NEG] {
        assert_eq!(h.debug_translate(&bus, bad_va), None);
        h.pc = CODE;
        h.x[10] = bad_va;
        h.step(&bus);
        assert_eq!(h.pc, STVEC);
        assert_eq!(h.csr_read(csr::SCAUSE).unwrap(), 13);
        assert_eq!(h.csr_read(csr::STVAL).unwrap(), bad_va);
    }
}

#[test]
fn pmp_m_mode_straddle_of_unlocked_subpage_entry_faults() {
    use rivemu_cpu_riscv::csr;

    let mut bus = AddressSpace::new();
    bus.add_ram("ram", 0x8000_0000, Arc::new(Ram::new(2 * MIB).unwrap()))
        .unwrap();

    const CODE: u64 = 0x8000_0000;
    const MTVEC: u64 = 0x8000_1000;
    const DATA: u64 = 0x8000_3000;

    // 0x00: ld x1, 8(x10)  (8-byte load at [DATA + 8, DATA + 15])
    // 0x04: lw x2, 12(x10) (4-byte load at [DATA + 12, DATA + 15], wholly inside unlocked NA4 pmp0)
    bus.write(CODE, AccessSize::Word, 0x0085_3083).unwrap();
    bus.write(CODE + 4, AccessSize::Word, 0x00c5_2103).unwrap();
    bus.write(DATA + 8, AccessSize::Double, 0x1122_3344_5566_7788)
        .unwrap();

    let mut h = Hart::new(0, Arc::new(FrozenTimebase(0)));
    h.reset(CODE);
    h.csr_write(csr::MTVEC, MTVEC).unwrap();
    // Configure pmp0 as an UNLOCKED (L=0) 4-byte NA4 entry at [DATA + 12, DATA + 15],
    // and pmp1 as a catch-all NAPOT R|W|X entry.
    h.csr_write(csr::PMPADDR0, (DATA + 12) >> 2).unwrap();
    h.csr_write(csr::PMPADDR0 + 1, 0x003f_ffff_ffff_ffff).unwrap();
    h.csr_write(csr::PMPCFG0, (0x1fu64 << 8) | 0x10).unwrap();

    // 1. 8-byte M-mode load at DATA + 8 straddles unlocked pmp0 [DATA + 12, DATA + 15]:
    // Per RISC-V Privileged Spec Section 3.7.1, a partial match fails irrespective of L/R/W/X.
    h.x[10] = DATA;
    h.step(&bus);
    assert_eq!(h.pc, MTVEC);
    assert_eq!(h.csr_read(csr::MCAUSE).unwrap(), 5);
    assert_eq!(h.csr_read(csr::MTVAL).unwrap(), DATA + 8);

    // 2. 4-byte M-mode load at DATA + 12 is wholly inside unlocked pmp0 (L=0, R=0):
    // M-mode bypasses permission checks on unlocked PMP entries when all bytes match.
    h.pc = CODE + 4;
    h.step(&bus);
    assert_eq!(h.pc, CODE + 8);
    assert_eq!(h.x[2], 0x1122_3344);
}




