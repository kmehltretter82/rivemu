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

#[test]
fn sscofpmf_and_smcntrpmf_filtering_overflow_scountovf_and_lcofi_interrupt() {
    use rivemu_cpu_riscv::csr::{self, hpmevent, irq, status};
    use rivemu_cpu_riscv::Priv;

    let mut bus = AddressSpace::new();
    bus.add_ram("ram", 0x8000_0000, Arc::new(Ram::new(2 * MIB).unwrap()))
        .unwrap();

    const CODE: u64 = 0x8000_0000;
    const STVEC: u64 = 0x8000_1000;

    // 4 NOPs at CODE, 1 NOP at STVEC
    for i in 0..4 {
        bus.write(CODE + i * 4, AccessSize::Word, 0x0000_0013).unwrap();
    }
    bus.write(STVEC, AccessSize::Word, 0x0000_0013).unwrap();

    let mut h = Hart::new(0, Arc::new(FrozenTimebase(0)));
    h.reset(CODE);

    // 1. Smcntrpmf mode filtering on minstret: set minstretcfg.MINH = 1, verify M-mode
    // does not increment minstret, while S-mode does.
    h.csr_write(csr::MINSTRETCFG, hpmevent::MINH).unwrap();
    assert_eq!(h.csr_read(csr::MINSTRET).unwrap(), 0);
    h.step(&bus);
    assert_eq!(h.csr_read(csr::MINSTRET).unwrap(), 0);

    h.prv = Priv::Supervisor;
    h.step(&bus);
    assert_eq!(h.csr_read(csr::MINSTRET).unwrap(), 1);

    // 2. Configure mhpmcounter3 (event = instructions, MINH = 1, SINH = 0) at u64::MAX - 1,
    // and mhpmcounter4 (event = cycles, SINH = 1) at 100.
    h.prv = Priv::Machine;
    h.csr_write(csr::MHPMEVENT3, hpmevent::MINH | hpmevent::EVENT_INSTRUCTIONS)
        .unwrap();
    h.csr_write(csr::MHPMCOUNTER3, u64::MAX - 1).unwrap();
    h.csr_write(csr::MHPMEVENT3 + 1, hpmevent::SINH | hpmevent::EVENT_CYCLES)
        .unwrap();
    h.csr_write(csr::MHPMCOUNTER3 + 1, 100).unwrap();

    // Stepping in M-mode inhibits mhpmcounter3 (MINH=1) and increments mhpmcounter4 (SINH=1, MINH=0).
    h.pc = CODE;
    h.step(&bus);
    assert_eq!(h.csr_read(csr::MHPMCOUNTER3).unwrap(), u64::MAX - 1);
    assert_eq!(h.csr_read(csr::MHPMCOUNTER3 + 1).unwrap(), 101);

    // 3. Delegate LCOFI (bit 13) to S-mode, enable SIE.LCOFIE and SSTATUS.SIE, and step in S-mode:
    // Step 1 advances mhpmcounter3 from u64::MAX - 1 -> u64::MAX (no overflow yet), while
    // mhpmcounter4 is inhibited in S-mode (SINH=1).
    h.csr_write(csr::MIDELEG, irq::LCOFIP).unwrap();
    h.csr_write(csr::MIE, irq::LCOFIP).unwrap();
    h.csr_write(csr::STVEC, STVEC).unwrap();
    let mstatus = h.csr_read(csr::MSTATUS).unwrap();
    h.csr_write(csr::MSTATUS, mstatus | status::SIE).unwrap();

    h.prv = Priv::Supervisor;
    h.pc = CODE;
    h.step(&bus);
    assert_eq!(h.csr_read(csr::MHPMCOUNTER3).unwrap(), u64::MAX);
    assert_eq!(h.csr_read(csr::MHPMCOUNTER3 + 1).unwrap(), 101);
    assert_eq!(h.csr_read(csr::MIP).unwrap() & irq::LCOFIP, 0);

    // Step 2 wraps mhpmcounter3 from u64::MAX -> 0, sets mhpmevent3.OF (bit 63),
    // sets mip.LCOFIP (bit 13), and on the next step delivers the S-mode LCOFI interrupt!
    h.step(&bus);
    assert_eq!(h.csr_read(csr::MHPMCOUNTER3).unwrap(), 0);
    assert_ne!(h.csr_read(csr::MHPMEVENT3).unwrap() & hpmevent::OF, 0);
    assert_ne!(h.csr_read(csr::MIP).unwrap() & irq::LCOFIP, 0);

    // Verify scountovf (0xda0) in S-mode is gated by mcounteren bit 3.
    assert_eq!(h.csr_read(csr::SCOUNTOVF).unwrap(), 0);
    h.prv = Priv::Machine;
    h.csr_write(csr::MCOUNTEREN, 1 << 3).unwrap();
    h.prv = Priv::Supervisor;
    assert_eq!(h.csr_read(csr::SCOUNTOVF).unwrap(), 1 << 3);

    // Next interrupt check in S-mode takes the delegated LCOFI interrupt
    // (scause = (1 << 63) | 13) to STVEC.
    h.take_pending_interrupt();
    assert_eq!(h.pc, STVEC);
    assert_eq!(h.prv, Priv::Supervisor);
    assert_eq!(h.csr_read(csr::SCAUSE).unwrap(), (1u64 << 63) | irq::LCOFI);

    // S-mode software can clear sip.LCOFIP via SIP write!
    h.csr_write(csr::SIP, 0).unwrap();
    assert_eq!(h.csr_read(csr::SIP).unwrap() & irq::LCOFIP, 0);
}

#[test]
fn zfa_fli_fminm_fmaxm_fround_fcvtmod_and_quiet_compares() {
    use rivemu_cpu_riscv::csr;

    let mut bus = AddressSpace::new();
    bus.add_ram("ram", 0x8000_0000, Arc::new(Ram::new(2 * MIB).unwrap()))
        .unwrap();

    const CODE: u64 = 0x8000_0000;

    // Program exercising Zfa instructions:
    //   0x00: fli.s      f1, -1.0       (rs1=0:  0xf010_00d3)
    //   0x04: fli.d      f2, 2.5        (rs1=21: 0xf21a_8153)
    //   0x08: fli.d      f3, nan        (rs1=31: 0xf21f_81d3)
    //   0x0c: fminm.d    f4, f2, f3     (0x2a31_2253) -> canonical NaN (unlike fmin.d)
    //   0x10: fround.d   f5, f2, rne    (0x4241_02d3) -> 2.0, NX=0
    //   0x14: froundnx.d f6, f2, rne    (0x4251_0353) -> 2.0, NX=1
    //   0x18: fleq.d     x1, f2, f3     (0xa231_40d3) -> 0, NV=0 (quiet NaN)
    //   0x1c: fltq.d     x2, f2, f3     (0xa231_5153) -> 0, NV=0 (quiet NaN)
    //   0x20: fcvtmod.w.d x3, f7, rtz   (0xc283_91d3) -> modulo 2^32 conversion
    let insns: &[u32] = &[
        0xf010_00d3,
        0xf21a_8153,
        0xf21f_81d3,
        0x2a31_2253,
        0x4241_02d3,
        0x4251_0353,
        0xa231_40d3,
        0xa231_5153,
        0xc283_91d3,
    ];
    for (i, &w) in insns.iter().enumerate() {
        bus.write(CODE + (i as u64) * 4, AccessSize::Word, u64::from(w))
            .unwrap();
    }

    let mut h = Hart::new(0, Arc::new(FrozenTimebase(0)));
    h.reset(CODE);
    // Enable FPU (FS = Initial)
    let mstatus = h.csr_read(csr::MSTATUS).unwrap();
    h.csr_write(csr::MSTATUS, mstatus | (1 << 13)).unwrap();

    // Step 0..4: fli.s, fli.d (2.5), fli.d (qNaN), fminm.d
    for _ in 0..4 {
        h.step(&bus);
    }
    assert_eq!(h.f[1], 0xffff_ffff_bf80_0000);
    assert_eq!(h.f[2], 0x4004_0000_0000_0000);
    assert_eq!(h.f[3], 0x7ff8_0000_0000_0000);
    assert_eq!(h.f[4], 0x7ff8_0000_0000_0000);
    assert_eq!(h.csr_read(csr::FFLAGS).unwrap(), 0);

    // Step 4: fround.d f5, f2, rne -> 2.0 (0x4000_0000_0000_0000), fflags == 0 (no NX)
    h.step(&bus);
    assert_eq!(h.f[5], 0x4000_0000_0000_0000);
    assert_eq!(h.csr_read(csr::FFLAGS).unwrap(), 0);

    // Step 5: froundnx.d f6, f2, rne -> 2.0, fflags == NX (1)
    h.step(&bus);
    assert_eq!(h.f[6], 0x4000_0000_0000_0000);
    assert_eq!(h.csr_read(csr::FFLAGS).unwrap(), 1);

    // Clear fflags, then step 6 & 7: fleq.d / fltq.d with quiet NaN do not set NV!
    h.csr_write(csr::FFLAGS, 0).unwrap();
    h.step(&bus);
    h.step(&bus);
    assert_eq!(h.x[1], 0);
    assert_eq!(h.x[2], 0);
    assert_eq!(h.csr_read(csr::FFLAGS).unwrap(), 0);

    // Step 8: fcvtmod.w.d on (2^32 + 0x8000_0005).75 = 4294967296.0 + 2147483653.75 = 6442450949.75
    // Truncated integer is 0x1_8000_0005; modulo 2^32 sign-extended to 64 bits is 0xffff_ffff_8000_0005,
    // and fflags is NV (16) without NX!
    h.f[7] = (6442450949.75f64).to_bits();
    h.step(&bus);
    assert_eq!(h.x[3], 0xffff_ffff_8000_0005);
    assert_eq!(h.csr_read(csr::FFLAGS).unwrap(), 16);
}

#[test]
fn zacas_zabha_and_zalasr_atomic_extensions() {
    use rivemu_cpu_riscv::csr;

    let mut bus = AddressSpace::new();
    bus.add_ram("ram", 0x8000_0000, Arc::new(Ram::new(MIB).unwrap()))
        .unwrap();

    const CODE: u64 = 0x8000_0000;
    const DATA: u64 = 0x8000_2000;
    const MTVEC: u64 = 0x8000_1000;

    let encode_amo = |f5: u32, aq: u32, rl: u32, rs2: u32, rs1: u32, f3: u32, rd: u32| -> u32 {
        (f5 << 27) | (aq << 26) | (rl << 25) | (rs2 << 20) | (rs1 << 15) | (f3 << 12) | (rd << 7) | 0x2f
    };

    let insns: &[u32] = &[
        // 0: amoadd.b  x5, x6, (x10)   -- byte at DATA: 0xfe (-2) + 5 = 3, x5 = sext8(0xfe) = -2
        encode_amo(0x00, 0, 0, 6, 10, 0, 5),
        // 1: amomin.b  x7, x8, (x10)   -- byte at DATA: 3 vs x8=0x80 (-128) -> -128 (0x80), x7 = 3
        encode_amo(0x10, 0, 0, 8, 10, 0, 7),
        // 2: amomaxu.h x9, x11, (x12)  -- half at DATA+2: 0x8001 vs x11=0x7fff -> stays 0x8001, x9 = sext16(0x8001)
        encode_amo(0x1c, 0, 0, 11, 12, 1, 9),
        // 3: amocas.w  x13, x14, (x15) -- word at DATA+4: 0x8000_0001; x13 has upper bits 0x1234_5678_8000_0001 -> matches low 32 bits!
        encode_amo(0x05, 1, 1, 14, 15, 2, 13),
        // 4: amocas.d  x0, x14, (x16)  -- double at DATA+8 is 0 -> matches x0=0, writes x14, x0 stays 0!
        encode_amo(0x05, 1, 1, 14, 16, 3, 0),
        // 5: amocas.d  x0, x6, (x16)   -- double at DATA+8 is now x14 (!= 0) -> mismatch against x0=0, memory stays x14!
        encode_amo(0x05, 1, 1, 6, 16, 3, 0),
        // 6: amocas.q  x18, x20, (x17) -- 128-bit at DATA+16: matches (x18, x19), writes (x20, x21), loads old into (x18, x19)
        encode_amo(0x05, 1, 1, 20, 17, 4, 18),
        // 7: sb.rl     x6, (x10)       -- Zalasr store-release byte (x6 = 0xf0) to DATA
        encode_amo(0x07, 0, 1, 6, 10, 0, 0),
        // 8: lb.aq     x22, (x10)      -- Zalasr load-acquire byte from DATA -> sext8(0xf0) = -16
        encode_amo(0x06, 1, 0, 0, 10, 0, 22),
        // 9: ld.aq     x0, (x16)       -- Zalasr load-acquire with rd=x0 -> x0 stays 0
        encode_amo(0x06, 1, 0, 0, 16, 3, 0),
    ];
    for (i, &w) in insns.iter().enumerate() {
        bus.write(CODE + (i as u64) * 4, AccessSize::Word, u64::from(w))
            .unwrap();
    }

    bus.write(DATA, AccessSize::Byte, 0xfe).unwrap();
    bus.write(DATA + 2, AccessSize::Half, 0x8001).unwrap();
    bus.write(DATA + 4, AccessSize::Word, 0x8000_0001).unwrap();
    bus.write(DATA + 8, AccessSize::Double, 0).unwrap();
    bus.write(DATA + 16, AccessSize::Double, 0x1111_2222_3333_4444)
        .unwrap();
    bus.write(DATA + 24, AccessSize::Double, 0x5555_6666_7777_8888)
        .unwrap();

    let mut h = Hart::new(0, Arc::new(FrozenTimebase(0)));
    h.reset(CODE);
    h.csr_write(csr::MTVEC, MTVEC).unwrap();

    h.x[6] = 5;
    h.x[8] = 0x80;
    h.x[10] = DATA;
    h.x[11] = 0x7fff;
    h.x[12] = DATA + 2;
    h.x[13] = 0x1234_5678_8000_0001;
    h.x[14] = 0xcafe_babe_dead_beef;
    h.x[15] = DATA + 4;
    h.x[16] = DATA + 8;
    h.x[17] = DATA + 16;
    h.x[18] = 0x1111_2222_3333_4444;
    h.x[19] = 0x5555_6666_7777_8888;
    h.x[20] = 0xaaaa_bbbb_cccc_dddd;
    h.x[21] = 0xeeee_ffff_0000_1111;

    // Steps 0..7
    for _ in 0..7 {
        h.step(&bus);
    }
    assert_eq!(h.x[5], (-2i64) as u64);
    assert_eq!(h.x[7], 3);
    assert_eq!(bus.read(DATA, AccessSize::Byte).unwrap(), 0x80);
    assert_eq!(h.x[9], 0xffff_ffff_ffff_8001);
    assert_eq!(h.x[13], 0xffff_ffff_8000_0001);
    assert_eq!(bus.read(DATA + 4, AccessSize::Word).unwrap(), 0xdead_beef);
    assert_eq!(h.x[0], 0);
    assert_eq!(
        bus.read(DATA + 8, AccessSize::Double).unwrap(),
        0xcafe_babe_dead_beef
    );
    assert_eq!(
        bus.read(DATA + 16, AccessSize::Double).unwrap(),
        0xaaaa_bbbb_cccc_dddd
    );
    assert_eq!(
        bus.read(DATA + 24, AccessSize::Double).unwrap(),
        0xeeee_ffff_0000_1111
    );

    // Steps 7..10: Zalasr sb.rl, lb.aq, ld.aq x0
    h.x[6] = 0xf0;
    for _ in 7..10 {
        h.step(&bus);
    }
    assert_eq!(h.x[22], (-16i64) as u64);
    assert_eq!(h.x[0], 0);

    // Odd register in amocas.q (rd = 5) raises ILLEGAL_INSN (mcause = 2).
    let odd_amocas_q = encode_amo(0x05, 0, 0, 20, 17, 4, 5);
    bus.write(CODE + 40, AccessSize::Word, u64::from(odd_amocas_q))
        .unwrap();
    h.pc = CODE + 40;
    h.step(&bus);
    assert_eq!(h.pc, MTVEC);
    assert_eq!(h.csr_read(csr::MCAUSE).unwrap(), 2);

    // Misaligned lh.aq raises LOAD_MISALIGNED (mcause = 4).
    let misaligned_lh_aq = encode_amo(0x06, 1, 0, 0, 10, 1, 5);
    bus.write(CODE + 44, AccessSize::Word, u64::from(misaligned_lh_aq))
        .unwrap();
    h.x[10] = DATA + 1;
    h.pc = CODE + 44;
    h.step(&bus);
    assert_eq!(h.pc, MTVEC);
    assert_eq!(h.csr_read(csr::MCAUSE).unwrap(), 4);
}

#[test]
fn zcb_compressed_loads_stores_exts_not_and_mul() {
    let mut bus = AddressSpace::new();
    bus.add_ram("ram", 0x8000_0000, Arc::new(Ram::new(MIB).unwrap()))
        .unwrap();

    const CODE: u64 = 0x8000_0000;
    const DATA: u64 = 0x8000_2000;

    // 16-bit Zcb instructions using compressed registers s0=x8 (0), s1=x9 (1), a0=x10 (2), a1=x11 (3), a2=x12 (4):
    //   0: c.sb     s1, 3(s0)   -- (4<<13)|(1<<6)|(1<<5)|(0<<7)|(1<<2)|0 = 0x8864
    //   1: c.sh     a0, 2(s0)   -- (4<<13)|(1<<10)|(0<<7)|(1<<6)|(0<<5)|(2<<2)|0 = 0x8448
    //   2: c.lbu    a1, 3(s0)   -- (4<<13)|(0<<7)|(1<<6)|(1<<5)|(3<<2)|0 = 0x806c
    //   3: c.lhu    a2, 2(s0)   -- (4<<13)|(0<<7)|(1<<6)|(0<<5)|(4<<2)|0 = 0x8050
    //   4: c.lh     a3, 2(s0)   -- (4<<13)|(1<<6)|(1<<5)|(0<<7)|(5<<2)|0 = 0x8074 (f3=4, bit10=1 -> 0x8454)
    //   5: c.zext.b s1          -- 0x9c61 | (1<<7) = 0x9ce1
    //   6: c.sext.b a0          -- 0x9c65 | (2<<7) = 0x9d65
    //   7: c.zext.h a1          -- 0x9c69 | (3<<7) = 0x9de9
    //   8: c.sext.h a2          -- 0x9c6d | (4<<7) = 0x9e6d
    //   9: c.zext.w a3          -- 0x9c71 | (5<<7) = 0x9ef1
    //  10: c.not    a4          -- 0x9c75 | (6<<7) = 0x9f75
    //  11: c.mul    a5, s1      -- 0x9c41 | (7<<7) | (1<<2) = 0x9fc5
    let halfwords: &[u16] = &[
        0x8864, // c.sb     s1, 3(s0)
        0x8c28, // c.sh     a0, 2(s0)
        0x806c, // c.lbu    a1, 3(s0)
        0x8430, // c.lhu    a2, 2(s0)
        0x8474, // c.lh     a3, 2(s0)
        0x9ce1, // c.zext.b s1
        0x9d65, // c.sext.b a0
        0x9de9, // c.zext.h a1
        0x9e6d, // c.sext.h a2
        0x9ef1, // c.zext.w a3
        0x9f75, // c.not    a4
        0x9fc5, // c.mul    a5, s1
    ];
    for (i, &hw) in halfwords.iter().enumerate() {
        bus.write(CODE + (i as u64) * 2, AccessSize::Half, u64::from(hw))
            .unwrap();
    }

    let mut h = Hart::new(0, Arc::new(FrozenTimebase(0)));
    h.reset(CODE);
    h.x[8] = DATA; // s0
    h.x[9] = 0x1234_5678_9abc_def1; // s1
    h.x[10] = 0xffff_ffff_ffff_80fe; // a0
    h.x[14] = 0x0000_ffff_0000_ffff; // a4
    h.x[15] = 7; // a5

    // Step 0: c.sb s1, 3(s0) -> byte at DATA+3 = 0xf1
    h.step(&bus);
    assert_eq!(bus.read(DATA + 3, AccessSize::Byte).unwrap(), 0xf1);
    // Step 1: c.sh a0, 2(s0) -> half at DATA+2 = 0x80fe (so DATA+2 = 0xfe, DATA+3 = 0x80)
    h.step(&bus);
    assert_eq!(bus.read(DATA + 2, AccessSize::Half).unwrap(), 0x80fe);
    // Step 2: c.lbu a1, 3(s0) -> a1 = 0x80
    h.step(&bus);
    assert_eq!(h.x[11], 0x80);
    // Step 3: c.lhu a2, 2(s0) -> a2 = 0x80fe
    h.step(&bus);
    assert_eq!(h.x[12], 0x80fe);
    // Step 4: c.lh a3, 2(s0) -> a3 = 0xffff_ffff_ffff_80fe
    h.step(&bus);
    assert_eq!(h.x[13], 0xffff_ffff_ffff_80fe);
    // Step 5: c.zext.b s1 -> s1 = 0xf1
    h.step(&bus);
    assert_eq!(h.x[9], 0xf1);
    // Step 6: c.sext.b a0 (low byte 0xfe = -2) -> 0xffff_ffff_ffff_fffe
    h.step(&bus);
    assert_eq!(h.x[10], 0xffff_ffff_ffff_fffe);
    // Step 7: c.zext.h a1 (0x1234_5678_9abc_def0 -> 0xdef0)
    h.x[11] = 0x1234_5678_9abc_def0;
    h.step(&bus);
    assert_eq!(h.x[11], 0xdef0);
    // Step 8: c.sext.h a2 (0x80fe -> 0xffff_ffff_ffff_80fe)
    h.step(&bus);
    assert_eq!(h.x[12], 0xffff_ffff_ffff_80fe);
    // Step 9: c.zext.w a3 (0xffff_ffff_ffff_80fe -> 0xffff_80fe)
    h.step(&bus);
    assert_eq!(h.x[13], 0xffff_80fe);
    // Step 10: c.not a4
    h.step(&bus);
    assert_eq!(h.x[14], !0x0000_ffff_0000_ffffu64);
    // Step 11: c.mul a5, s1 (7 * 0xf1 = 1687)
    h.step(&bus);
    assert_eq!(h.x[15], 7 * 0xf1);
}

#[test]
fn macro_op_fusion_and_direct_offset_soft_tlb() {
    let mut bus = AddressSpace::new();
    bus.add_ram("ram", 0x8000_0000, Arc::new(Ram::new(MIB).unwrap()))
        .unwrap();

    const CODE: u64 = 0x8000_0000;
    const TARGET: u64 = 0x8000_0100;

    // 5 macro-op fused pairs at CODE (10 instructions total):
    //   Pair 1 (AuipcAddi):
    //     0x00: auipc x5, 2          (0x0000_2297)
    //     0x04: addi  x5, x5, 0x123  (0x1232_8293) -> x5 = CODE + 0x2123
    //   Pair 2 (LuiAddi):
    //     0x08: lui   x6, 0x12345    (0x1234_5337)
    //     0x0c: addi  x6, x6, 0x678  (0x6783_0313) -> x6 = 0x1234_5678
    //   Pair 3 (LuiAddiw):
    //     0x10: lui   x7, 0x80001    (0x8000_13b7)
    //     0x14: addiw x7, x7, -1     (0xfff3_839b) -> x7 = 0xffff_ffff_8000_0fff
    //   Pair 4 (ZextW: slli rd, rs1, 32 + srli rd, rd, 32):
    //     0x18: slli  x8, x7, 32     (0x0203_9413)
    //     0x1c: srli  x8, x8, 32     (0x0204_5413) -> x8 = 0x8000_0fff
    //   Pair 5 (AuipcJalr):
    //     0x20: auipc x1, 0          (0x0000_0097)
    //     0x24: jalr  x1, 0xe0(x1)   (0x0e00_80e7) -> x1 = CODE + 0x28, pc = CODE + 0x100 = TARGET
    let insns: &[u32] = &[
        0x0000_2297,
        0x1232_8293,
        0x1234_5337,
        0x6783_0313,
        0x8000_13b7,
        0xfff3_839b,
        0x0203_9413,
        0x0204_5413,
        0x0000_0097,
        0x0e00_80e7,
    ];
    for (i, &w) in insns.iter().enumerate() {
        bus.write(CODE + (i as u64) * 4, AccessSize::Word, u64::from(w))
            .unwrap();
    }

    // At TARGET (0x8000_0100): sd x6, 0(x5); ld x9, 0(x5) (exercises Direct-Offset SoftTLB)
    bus.write(TARGET, AccessSize::Word, 0x0062_b023).unwrap();
    bus.write(TARGET + 4, AccessSize::Word, 0x0002_b483).unwrap();

    // Run with Hart::run(budget = 12) which executes all 5 fused pairs in 5 fused dispatches + 2 memory ops:
    let mut h_fused = Hart::new(0, Arc::new(FrozenTimebase(0)));
    h_fused.reset(CODE);
    let executed = h_fused.run(&bus, 12);
    assert_eq!(executed, 12);
    assert_eq!(h_fused.instret(), 12);
    assert_eq!(h_fused.x[5], CODE + 0x2123);
    assert_eq!(h_fused.x[6], 0x1234_5678);
    assert_eq!(h_fused.x[7], 0xffff_ffff_8000_0fff);
    assert_eq!(h_fused.x[8], 0x8000_0fff);
    assert_eq!(h_fused.x[1], CODE + 0x28);
    assert_eq!(h_fused.x[9], 0x1234_5678);
    assert_eq!(h_fused.pc, TARGET + 8);

    // Compare against 12 individual Hart::step() calls:
    bus.write(CODE + 0x2123, AccessSize::Double, 0).unwrap();
    let mut h_step = Hart::new(0, Arc::new(FrozenTimebase(0)));
    h_step.reset(CODE);
    for _ in 0..12 {
        h_step.step(&bus);
    }
    assert_eq!(h_fused.x, h_step.x);
    assert_eq!(h_fused.pc, h_step.pc);
    assert_eq!(h_fused.instret(), h_step.instret());
}







