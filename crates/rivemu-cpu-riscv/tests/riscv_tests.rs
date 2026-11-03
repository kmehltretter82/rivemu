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
    // Console output from the "v" environment (assertion messages, for example).
    let mut console = String::new();
    while executed < MAX_INSNS {
        executed += hart.run(&bus, 1000).max(1);
        let v = bus.read(tohost, AccessSize::Double).unwrap();
        if v == 0 {
            continue;
        }
        if v >> 48 == 0x0101 {
            console.push(char::from(v as u8));
            bus.write(tohost, AccessSize::Double, 0).unwrap();
            continue;
        }
        return if v == 1 {
            Ok(executed)
        } else {
            Err(format!(
                "failed test case #{} (pc={:#x}){}",
                v >> 1,
                hart.pc,
                console_suffix(&console)
            ))
        };
    }
    Err(format!(
        "timeout after {MAX_INSNS} instructions (pc={:#x}){}",
        hart.pc,
        console_suffix(&console)
    ))
}

/// Formats captured "v" environment console output for a failure message.
fn console_suffix(console: &str) -> String {
    let console = console.trim();
    if console.is_empty() {
        String::new()
    } else {
        format!("; console: {console:?}")
    }
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
    bus.write(
        ROOT_PT + 2 * 8,
        AccessSize::Double,
        ((L1_PT >> 12) << 10) | 1,
    )
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

    bus.write(PA_LOCAL_1, AccessSize::Word, 0x1111_1111)
        .unwrap();
    bus.write(PA_LOCAL_2, AccessSize::Word, 0x2222_2222)
        .unwrap();
    bus.write(PA_GLOBAL_1, AccessSize::Word, 0xaaaa_1111)
        .unwrap();
    bus.write(PA_GLOBAL_2, AccessSize::Word, 0xaaaa_2222)
        .unwrap();
    bus.write(PA_NONLEAF_1, AccessSize::Word, 0xbbbb_1111)
        .unwrap();
    bus.write(PA_NONLEAF_2, AccessSize::Word, 0xbbbb_2222)
        .unwrap();

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
    bus.write(
        ROOT_PT1 + 2 * 8,
        AccessSize::Double,
        ((L1_PT1 >> 12) << 10) | 0x01,
    )
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
    bus.write(0x8020_1000, AccessSize::Word, 0x5555_1111)
        .unwrap();
    bus.write(0x8000_1000, AccessSize::Word, 0x5555_2222)
        .unwrap();
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
    bus.write(
        L1_POS_PT,
        AccessSize::Double,
        ((L0_POS_PT >> 12) << 10) | 0x01,
    )
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
    h.csr_write(csr::PMPADDR0 + 1, 0x003f_ffff_ffff_ffff)
        .unwrap();
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
    use rivemu_cpu_riscv::Priv;
    use rivemu_cpu_riscv::csr::{self, hpmevent, irq, status};

    let mut bus = AddressSpace::new();
    bus.add_ram("ram", 0x8000_0000, Arc::new(Ram::new(2 * MIB).unwrap()))
        .unwrap();

    const CODE: u64 = 0x8000_0000;
    const STVEC: u64 = 0x8000_1000;

    // 4 NOPs at CODE, 1 NOP at STVEC
    for i in 0..4 {
        bus.write(CODE + i * 4, AccessSize::Word, 0x0000_0013)
            .unwrap();
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
    h.csr_write(
        csr::MHPMEVENT3,
        hpmevent::MINH | hpmevent::EVENT_INSTRUCTIONS,
    )
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
        (f5 << 27)
            | (aq << 26)
            | (rl << 25)
            | (rs2 << 20)
            | (rs1 << 15)
            | (f3 << 12)
            | (rd << 7)
            | 0x2f
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
    bus.write(TARGET + 4, AccessSize::Word, 0x0002_b483)
        .unwrap();

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

#[test]
fn zimop_zcmop_and_zicfiss_shadow_stack() {
    use rivemu_cpu_riscv::{Priv, cause, csr};

    let mut bus = AddressSpace::new();
    bus.add_ram("ram", 0x8000_0000, Arc::new(Ram::new(MIB).unwrap()))
        .unwrap();

    const CODE: u64 = 0x8000_0000;
    const PT_ROOT: u64 = 0x8000_2000;
    const PT_L1: u64 = 0x8000_3000;
    const PT_L0: u64 = 0x8000_4000;
    const SS_PAGE: u64 = 0x8000_5000;
    const RW_PAGE: u64 = 0x8000_6000;

    // Identity-map 0x8000_0000..0x8001_0000 via 3-level 4 KiB Sv39 page tables:
    //   VPN[2] = 2 -> PT_L1
    //   VPN[1] = 0 -> PT_L0
    //   VPN[0] = 0: CODE (R|W|X|A|D|V = 0xcf)
    //   VPN[0] = 5: SS_PAGE (shadow stack: R=0, W=1, X=0, A=1, D=1, V=1 = 0xc5)
    //   VPN[0] = 6: RW_PAGE (ordinary RW: R=1, W=1, X=0, A=1, D=1, V=1 = 0xc7)
    bus.write(PT_ROOT + 2 * 8, AccessSize::Double, (PT_L1 >> 12) << 10 | 1)
        .unwrap();
    bus.write(PT_L1, AccessSize::Double, (PT_L0 >> 12) << 10 | 1)
        .unwrap();
    bus.write(PT_L0, AccessSize::Double, (CODE >> 12) << 10 | 0xcf)
        .unwrap();
    bus.write(
        PT_L0 + 5 * 8,
        AccessSize::Double,
        (SS_PAGE >> 12) << 10 | 0xc5,
    )
    .unwrap();
    bus.write(
        PT_L0 + 6 * 8,
        AccessSize::Double,
        (RW_PAGE >> 12) << 10 | 0xc7,
    )
    .unwrap();

    let mut h = Hart::new(0, Arc::new(FrozenTimebase(0)));
    h.reset(CODE);
    h.csr.mtvec = 0x8000_0800;

    // 1. senvcfg.SSE cannot be set while menvcfg.SSE == 0, and clearing menvcfg.SSE clears senvcfg.SSE.
    h.csr_write(csr::SENVCFG, csr::envcfg::SSE).unwrap();
    assert_eq!(h.csr_read(csr::SENVCFG).unwrap() & csr::envcfg::SSE, 0);
    h.csr_write(csr::MENVCFG, csr::envcfg::SSE | csr::envcfg::CBCFE)
        .unwrap();
    h.csr_write(csr::SENVCFG, csr::envcfg::SSE).unwrap();
    assert_ne!(h.csr_read(csr::SENVCFG).unwrap() & csr::envcfg::SSE, 0);
    h.csr_write(csr::MENVCFG, csr::envcfg::CBCFE).unwrap();
    assert_eq!(h.csr_read(csr::SENVCFG).unwrap() & csr::envcfg::SSE, 0);

    // 2. M-mode: mop.r.0, mop.rr.0, c.mop.1, and ssrdp write 0; ssamoswap.d raises ILLEGAL_INSN with mtval = raw.
    h.csr_write(csr::MENVCFG, csr::envcfg::SSE | csr::envcfg::CBCFE)
        .unwrap();
    h.csr_write(csr::SSP, SS_PAGE + 0x103).unwrap();
    assert_eq!(h.csr_read(csr::SSP).unwrap(), SS_PAGE + 0x100); // [1:0] masked to 0
    h.x[5] = 0xdead_beef;
    h.x[7] = 0xcafe_babe;
    h.x[10] = 0x1111_2222;
    h.x[1] = SS_PAGE + 0x100;
    let m_insns: &[u32] = &[
        0x81c3_42f3, // mop.r.0 x5, x6 -> x5 = 0
        0x8294_43f3, // mop.rr.0 x7, x8, x9 -> x7 = 0
        0xcdc0_4573, // ssrdp x10 (in M-mode acts as MOP -> x10 = 0)
        0x4820_b1af, // ssamoswap.d x3, x2, (x1) (in M-mode -> ILLEGAL_INSN)
    ];
    for (i, &w) in m_insns.iter().enumerate() {
        bus.write(CODE + (i as u64) * 4, AccessSize::Word, u64::from(w))
            .unwrap();
    }
    for _ in 0..3 {
        h.step(&bus);
    }
    assert_eq!(h.x[5], 0);
    assert_eq!(h.x[7], 0);
    assert_eq!(h.x[10], 0);
    h.step(&bus);
    assert_eq!(h.pc, 0x8000_0800);
    assert_eq!(h.csr.mcause, cause::ILLEGAL_INSN);
    assert_eq!(h.csr.mtval, 0x4820_b1af);

    // 3. S-mode with Sv39 active and menvcfg.SSE = 1:
    const S_CODE: u64 = CODE + 0x100;
    h.prv = Priv::Supervisor;
    h.csr_write(csr::SATP, (8u64 << 60) | (PT_ROOT >> 12))
        .unwrap();
    h.csr_write(csr::MENVCFG, csr::envcfg::SSE | csr::envcfg::CBCFE)
        .unwrap();
    h.pc = S_CODE;
    h.x[1] = 0x1234_5678_9abc_def0;
    h.x[5] = 0xfedc_ba98_7654_3210;
    h.x[6] = SS_PAGE + 0xf0;
    h.x[7] = 0xaaaa_bbbb_cccc_dddd;
    h.x[8] = RW_PAGE;

    // Sequence in S-mode:
    //   0: sspush x1         (0xce10_4073) -> [SS_PAGE + 0xf8] = x1, ssp = SS_PAGE + 0xf8
    //   1: sspush x5         (0xce50_4073) -> [SS_PAGE + 0xf0] = x5, ssp = SS_PAGE + 0xf0
    //   2: ssrdp x10         (0xcdc0_4573) -> x10 = SS_PAGE + 0xf0
    //   3: ld x11, 0(x6)     (0x0003_3583) -> x11 = x5 (ordinary load Permitted, warms TLB)
    //   4: cbo.flush 0(x6)   (0x0023_200f) -> MUST fault with STORE_ACCESS (mcause=7) even on warm TLB!
    //   5: sd x7, 0(x6)      (0x0073_3023) -> MUST fault with STORE_ACCESS (mcause=7)
    //   6: ssamoswap.d x12, x7, (x6) (0x4873_362f) -> x12 = x5, [SS_PAGE + 0xf0] = x7
    //   7: ssamoswap.d x13, x7, (x8) (0x4874_36af) -> RW_PAGE is not shadow stack -> STORE_PAGE_FAULT (mcause=15)
    //   8: sspopchk x5       (0xcdc2_c073) -> [SS_PAGE + 0xf0] is now x7 != x5 -> SW_CHECK (mcause=18, mtval=3)
    //   9: sspopchk x1       (0xcdc0_c073)
    let s_insns: &[u32] = &[
        0xce10_4073, // 0: sspush x1
        0xce50_4073, // 1: sspush x5
        0xcdc0_4573, // 2: ssrdp x10
        0x0003_3583, // 3: ld x11, 0(x6)
        0x0023_200f, // 4: cbo.flush 0(x6)
        0x0073_3023, // 5: sd x7, 0(x6)
        0x4873_362f, // 6: ssamoswap.d x12, x7, (x6)
        0x4874_36af, // 7: ssamoswap.d x13, x7, (x8)
        0xcdc2_c073, // 8: sspopchk x5
        0xcdc0_c073, // 9: sspopchk x1
    ];
    for (i, &w) in s_insns.iter().enumerate() {
        bus.write(S_CODE + (i as u64) * 4, AccessSize::Word, u64::from(w))
            .unwrap();
    }

    // Execute 0..4 (sspush x1, sspush x5, ssrdp x10, ld x11, 0(x6)):
    for _ in 0..4 {
        h.step(&bus);
    }
    assert_eq!(h.x[10], SS_PAGE + 0xf0);
    assert_eq!(h.x[11], 0xfedc_ba98_7654_3210);

    // Step 4: cbo.flush 0(x6) on warm TLB must raise STORE_ACCESS (mcause=7, mtval=SS_PAGE+0xf0):
    h.step(&bus);
    assert_eq!(h.csr.mcause, cause::STORE_ACCESS);
    assert_eq!(h.csr.mtval, SS_PAGE + 0xf0);

    // Step 5: sd x7, 0(x6) must raise STORE_ACCESS (mcause=7, mtval=SS_PAGE+0xf0):
    h.prv = Priv::Supervisor;
    h.pc = S_CODE + 5 * 4;
    h.step(&bus);
    assert_eq!(h.csr.mcause, cause::STORE_ACCESS);
    assert_eq!(h.csr.mtval, SS_PAGE + 0xf0);

    // Step 6: ssamoswap.d x12, x7, (x6) succeeds:
    h.prv = Priv::Supervisor;
    h.pc = S_CODE + 6 * 4;
    h.step(&bus);
    assert_eq!(h.x[12], 0xfedc_ba98_7654_3210);

    // Step 7: ssamoswap.d x13, x7, (x8) on ordinary RW page raises STORE_ACCESS (mcause=7):
    h.step(&bus);
    assert_eq!(h.csr.mcause, cause::STORE_ACCESS);
    assert_eq!(h.csr.mtval, RW_PAGE);

    // Step 8: sspopchk x5 with x5 != [SS_PAGE + 0xf0] raises SW_CHECK (mcause=18, mtval=3):
    h.prv = Priv::Supervisor;
    h.pc = S_CODE + 8 * 4;
    h.step(&bus);
    assert_eq!(h.csr.mcause, cause::SW_CHECK);
    assert_eq!(h.csr.mtval, 3);
    assert_eq!(h.csr.ssp, SS_PAGE + 0xf0); // ssp unchanged on fault

    // Update x5 to match x7 and re-run sspopchk x5 + sspopchk x1:
    h.prv = Priv::Supervisor;
    h.x[5] = h.x[7];
    h.pc = S_CODE + 8 * 4;
    h.step(&bus);
    assert_eq!(h.csr.ssp, SS_PAGE + 0xf8);
    h.step(&bus);
    assert_eq!(h.csr.ssp, SS_PAGE + 0x100);

    // 4. M-mode with MPRV=1, MPP=S on cold TLB: LD from shadow stack page succeeds, SD raises STORE_ACCESS (mcause=7):
    h.prv = Priv::Machine;
    h.csr.mstatus = csr::status::MPRV | (1 << csr::status::MPP_SHIFT);
    h.csr_write(csr::MENVCFG, csr::envcfg::SSE | csr::envcfg::CBCFE)
        .unwrap();
    h.pc = S_CODE + 3 * 4; // ld x11, 0(x6)
    h.x[11] = 0;
    h.step(&bus);
    assert_eq!(h.x[11], h.x[7]);
    h.pc = S_CODE + 5 * 4; // sd x7, 0(x6)
    h.step(&bus);
    assert_eq!(h.csr.mcause, cause::STORE_ACCESS);
    assert_eq!(h.csr.mtval, SS_PAGE + 0xf0);
}

#[test]
fn svukte_zjpm_and_zicfilp_semantics() {
    use rivemu_cpu_riscv::{Priv, cause, csr};

    let mut bus = AddressSpace::new();
    bus.add_ram("ram", 0x8000_0000, Arc::new(Ram::new(MIB).unwrap()))
        .unwrap();
    let mut h = Hart::new(0, Arc::new(FrozenTimebase(0)));
    h.reset(0x8002_1000);
    // Firmware enables the stateen-gated S-mode state (mstateen resets to 0).
    h.csr_write(csr::MSTATEEN0, csr::stateen::M0_MASK).unwrap();
    h.csr.mtvec = 0x8000_0800;
    const PT_ROOT: u64 = 0x8002_0000;
    const CODE_PA: u64 = 0x8002_1000;
    const DATA_PA: u64 = 0x8002_2000;
    const HIGH_VA: u64 = 0xffff_ffc0_0000_0000; // VPN[2] = 256 (1 GiB leaf in upper half of Sv39)

    // Leaf 1 GiB PTE[2] -> identity maps 0x8000_0000..0xbfff_ffff with U|R|W|X|A|D
    bus.write(
        PT_ROOT + 2 * 8,
        AccessSize::Double,
        ((0x8000_0000u64 >> 12) << 10) | 0xdf,
    )
    .unwrap();
    // Leaf 1 GiB PTE[3] -> maps 0xc000_0000..0xffff_ffff to 0x8000_0000 with S-mode R|W|X|A|D (U=0)
    bus.write(
        PT_ROOT + 3 * 8,
        AccessSize::Double,
        ((0x8000_0000u64 >> 12) << 10) | 0xcf,
    )
    .unwrap();
    // Leaf 1 GiB PTE[256] -> maps HIGH_VA (0xffff_ffc0_0000_0000) to 0x8000_0000 with U|R|W|X|A|D
    bus.write(
        PT_ROOT + 256 * 8,
        AccessSize::Double,
        ((0x8000_0000u64 >> 12) << 10) | 0xdf,
    )
    .unwrap();
    bus.write(DATA_PA, AccessSize::Double, 0xdead_beef_cafe_babe)
        .unwrap();

    h.csr_write(csr::SATP, (8u64 << 60) | (PT_ROOT >> 12))
        .unwrap();

    // 1. Svukte: with senvcfg.UKTE == 0, U-mode load from HIGH_VA + 0x22000 succeeds and warms the TLB.
    let high_data_va = HIGH_VA + (DATA_PA - 0x8000_0000);
    bus.write(CODE_PA, AccessSize::Word, 0x0005_3583).unwrap(); // ld x11, 0(x10)
    h.prv = Priv::User;
    h.pc = CODE_PA;
    h.x[10] = high_data_va;
    h.step(&bus);
    assert_eq!(h.x[11], 0xdead_beef_cafe_babe);

    // Enable senvcfg.UKTE: U-mode load from high_data_va must now raise LOAD_PAGE_FAULT (13),
    // while S-mode (with SUM=1) still succeeds.
    h.csr_write(csr::SENVCFG, csr::envcfg::UKTE).unwrap();
    h.prv = Priv::User;
    h.pc = CODE_PA;
    h.x[11] = 0;
    h.step(&bus);
    assert_eq!(h.csr.mcause, cause::LOAD_PAGE_FAULT);
    assert_eq!(h.csr.mtval, high_data_va);

    h.prv = Priv::Machine;
    h.csr.mstatus |= csr::status::MPRV | (1 << csr::status::MPP_SHIFT) | csr::status::SUM;
    h.pc = CODE_PA;
    h.step(&bus);
    assert_eq!(h.x[11], 0xdead_beef_cafe_babe);
    h.csr.mstatus &= !(csr::status::MPRV | csr::status::MPP);

    // 2. Ssnpm / Smnpm pointer masking (PMLEN = 7 and PMLEN = 16):
    // Reserved PMM = 1 must be ignored (WARL preserves old PMM = 0).
    h.csr_write(csr::SENVCFG, 1u64 << csr::envcfg::PMM_SHIFT)
        .unwrap();
    assert_eq!(h.csr.senvcfg & csr::envcfg::PMM_MASK, 0);

    // Set senvcfg.PMM = PMLEN_7 (2 << 32): top 7 bits of U-mode data pointer are sign-extended from bit 56.
    h.csr_write(csr::SENVCFG, csr::envcfg::PMM_PMLEN_7).unwrap();
    h.prv = Priv::User;
    h.pc = CODE_PA;
    h.x[10] = (0x5au64 << 57) | DATA_PA; // bit 56 is 0 -> sign-extends to DATA_PA
    h.x[11] = 0;
    h.step(&bus);
    assert_eq!(h.x[11], 0xdead_beef_cafe_babe);

    // When mstatus.MXR == 1, pointer masking is bypassed for U/S mode -> non-canonical address faults with full VA in mtval.
    h.csr.mstatus |= csr::status::MXR;
    h.prv = Priv::User;
    h.pc = CODE_PA;
    h.step(&bus);
    assert_eq!(h.csr.mcause, cause::LOAD_PAGE_FAULT);
    assert_eq!(h.csr.mtval, (0x5au64 << 57) | DATA_PA);
    h.csr.mstatus &= !csr::status::MXR;

    // 3. Zicfilp forward-edge CFI (lpad + ELP + SPELP):
    // Enable senvcfg.LPE for U-mode and delegate SW_CHECK (18) to S-mode:
    h.csr_write(csr::SENVCFG, csr::envcfg::LPE).unwrap();
    h.csr.medeleg = 1 << cause::SW_CHECK;
    h.csr.stvec = 0xc002_1200; // S-mode alias (U=0) of CODE_PA + 0x200

    // Code at CODE_PA + 0x40:
    //   +0x40: jalr x1, 0(x6)      (0x0003_00e7) -> indirect call via x6 sets ELP = true
    //   +0x80: lpad 0xabcde        (auipc x0, 0xabcde = 0xabcd_e017)
    //   +0x84: addi x12, x0, 42    (0x02a0_0613)
    //   +0xc0: addi x12, x0, 99    (0x0630_0613) -> non-lpad target
    bus.write(CODE_PA + 0x40, AccessSize::Word, 0x0003_00e7)
        .unwrap();
    bus.write(CODE_PA + 0x80, AccessSize::Word, 0xabcd_e017)
        .unwrap();
    bus.write(CODE_PA + 0x84, AccessSize::Word, 0x02a0_0613)
        .unwrap();
    bus.write(CODE_PA + 0xc0, AccessSize::Word, 0x0630_0613)
        .unwrap();
    bus.write(CODE_PA + 0x200, AccessSize::Word, 0x1020_0073) // sret at stvec
        .unwrap();

    // Case A: matching label in x7[31:12] = 0xabcde succeeds and clears ELP:
    h.prv = Priv::User;
    h.pc = CODE_PA + 0x40;
    h.x[6] = CODE_PA + 0x80;
    h.x[7] = 0xabcd_e000;
    h.step(&bus); // jalr x1, 0(x6)
    assert!(h.elp);
    h.step(&bus); // lpad 0xabcde
    assert!(!h.elp);
    h.step(&bus); // addi x12, x0, 42
    assert_eq!(h.x[12], 42);

    // Case B: mismatched label in x7[31:12] raises SW_CHECK (scause=18, stval=2) and saves SPELP=1 in sstatus:
    h.prv = Priv::User;
    h.pc = CODE_PA + 0x40;
    h.x[7] = 0x1234_5000;
    h.step(&bus); // jalr x1, 0(x6)
    assert!(h.elp);
    h.step(&bus); // lpad 0xabcde -> mismatch -> trap to S-mode
    assert_eq!(h.prv, Priv::Supervisor);
    assert_eq!(h.csr.scause, cause::SW_CHECK);
    assert_eq!(h.csr.stval, 2);
    assert_ne!(h.csr.mstatus & csr::status::SPELP, 0);
    assert!(!h.elp);

    // Fix x7 in the trap handler and execute sret: SPELP is restored to h.elp and cleared in mstatus,
    // so the retried lpad succeeds!
    h.x[7] = 0xabcd_e000;
    h.step(&bus); // sret
    assert_eq!(h.prv, Priv::User);
    assert_eq!(h.csr.mstatus & csr::status::SPELP, 0);
    assert!(h.elp);
    h.step(&bus); // lpad 0xabcde now succeeds!
    assert!(!h.elp);

    // Case C: indirect jump to non-lpad instruction raises SW_CHECK (scause=18, stval=2):
    h.prv = Priv::User;
    h.pc = CODE_PA + 0x40;
    h.x[6] = CODE_PA + 0xc0;
    h.step(&bus); // jalr x1, 0(x6)
    h.step(&bus); // addi at +0xc0 -> SW_CHECK (stval=2)
    assert_eq!(h.csr.scause, cause::SW_CHECK);
    assert_eq!(h.csr.stval, 2);
}

#[test]
fn svnapot_sv57_and_zawrs_semantics() {
    use rivemu_cpu_riscv::{Priv, cause, csr};

    let mut bus = AddressSpace::new();
    bus.add_ram("ram", 0x8000_0000, Arc::new(Ram::new(2 * MIB).unwrap()))
        .unwrap();
    let mut h = Hart::new(0, Arc::new(FrozenTimebase(0)));
    h.reset(0x8002_0000);
    h.csr.mtvec = 0x8000_0100;

    // 5-level Sv57 page table setup:
    //   L4 root at 0x8001_0000
    //   L3 at      0x8001_1000
    //   L2 at      0x8001_2000
    //   L1 at      0x8001_3000
    //   L0 at      0x8001_4000
    const L4: u64 = 0x8001_0000;
    const L3: u64 = 0x8001_1000;
    const L2: u64 = 0x8001_2000;
    const L1: u64 = 0x8001_3000;
    const L0: u64 = 0x8001_4000;
    const CODE_PA: u64 = 0x8002_0000;
    const NAPOT_BASE_PA: u64 = 0x8004_0000; // 64 KiB aligned
    const ALT_BASE_PA: u64 = 0x8006_0000; // 64 KiB aligned
    const NAPOT_BASE_VA: u64 = 0x8004_0000;
    const PTE_N: u64 = 1 << 63;

    // Identity-map L4[0] -> L3[0] -> L2[2] -> L1[0] -> L0:
    bus.write(L4, AccessSize::Double, ((L3 >> 12) << 10) | 0x01)
        .unwrap();
    bus.write(L3, AccessSize::Double, ((L2 >> 12) << 10) | 0x01)
        .unwrap();
    bus.write(L2 + 2 * 8, AccessSize::Double, ((L1 >> 12) << 10) | 0x01)
        .unwrap();
    bus.write(L1, AccessSize::Double, ((L0 >> 12) << 10) | 0x01)
        .unwrap();
    // Map CODE_PA (vpn0 = 0x20) as R|W|X|A|D:
    bus.write(
        L0 + 0x20 * 8,
        AccessSize::Double,
        ((CODE_PA >> 12) << 10) | 0xcf,
    )
    .unwrap();

    // Install 64 KiB Svnapot leaf PTEs at L0[0x40..0x50] mapping NAPOT_BASE_VA -> NAPOT_BASE_PA:
    // ppn[3:0] must be 0b1000 (0x8).
    let napot_ppn = ((NAPOT_BASE_PA >> 12) & !0xf) | 0x8;
    let napot_pte = PTE_N | (napot_ppn << 10) | 0xc7;
    for i in 0..16u64 {
        bus.write(L0 + (0x40 + i) * 8, AccessSize::Double, napot_pte)
            .unwrap();
        bus.write(
            NAPOT_BASE_PA + i * 0x1000,
            AccessSize::Double,
            0x1111_0000_0000_0000 | i,
        )
        .unwrap();
        bus.write(
            ALT_BASE_PA + i * 0x1000,
            AccessSize::Double,
            0x2222_0000_0000_0000 | i,
        )
        .unwrap();
    }

    // Also install a high Sv57 256 TiB superpage leaf at L4[511] (VA 0xffff_0000_0000_0000 -> PA 0x0):
    bus.write(L4 + 511 * 8, AccessSize::Double, 0xc7).unwrap();

    // Enable Sv57 (MODE = 10):
    h.csr_write(csr::SATP, (10u64 << 60) | (L4 >> 12)).unwrap();
    assert_eq!(h.csr.satp >> 60, 10);

    // 1. Svnapot 64 KiB translation & TLB invalidation across the 64 KiB range:
    bus.write(CODE_PA, AccessSize::Word, 0x0005_3583).unwrap(); // ld x11, 0(x10)
    bus.write(CODE_PA + 4, AccessSize::Word, 0x1205_0073)
        .unwrap(); // sfence.vma x10, x0
    h.prv = Priv::Supervisor;

    // Warm TLB at offset +0x0000 and +0xf000 within the 64 KiB NAPOT region:
    h.pc = CODE_PA;
    h.x[10] = NAPOT_BASE_VA;
    h.step(&bus);
    assert_eq!(h.x[11], 0x1111_0000_0000_0000);

    h.pc = CODE_PA;
    h.x[10] = NAPOT_BASE_VA + 0xf000;
    h.step(&bus);
    assert_eq!(h.x[11], 0x1111_0000_0000_000f);

    // Rewrite PTEs to point to ALT_BASE_PA, then execute sfence.vma on NAPOT_BASE_VA + 0x5000:
    // Because the cached entry at +0xf000 has vpn_shift = 4 (64 KiB), sfence.vma on +0x5000 must flush it!
    let alt_napot_ppn = ((ALT_BASE_PA >> 12) & !0xf) | 0x8;
    let alt_napot_pte = PTE_N | (alt_napot_ppn << 10) | 0xc7;
    for i in 0..16u64 {
        bus.write(L0 + (0x40 + i) * 8, AccessSize::Double, alt_napot_pte)
            .unwrap();
    }
    h.pc = CODE_PA + 4;
    h.x[10] = NAPOT_BASE_VA + 0x5000;
    h.step(&bus); // sfence.vma x10, x0

    h.pc = CODE_PA;
    h.x[10] = NAPOT_BASE_VA + 0xf000;
    h.step(&bus);
    assert_eq!(h.x[11], 0x2222_0000_0000_000f);

    // Reserved Svnapot encoding (N=1 with ppn[3:0] != 8) must raise LOAD_PAGE_FAULT (13):
    let bad_napot_pte = PTE_N | (((NAPOT_BASE_PA >> 12) & !0xf | 0x4) << 10) | 0xc7;
    bus.write(L0 + 0x50 * 8, AccessSize::Double, bad_napot_pte)
        .unwrap();
    h.prv = Priv::Supervisor;
    h.pc = CODE_PA;
    h.x[10] = 0x8005_0000;
    h.step(&bus);
    assert_eq!(h.csr.mcause, cause::LOAD_PAGE_FAULT);
    assert_eq!(h.csr.mtval, 0x8005_0000);

    // 2. Sv57 256 TiB superpage at 0xffff_0000_8002_0000 and non-canonical 57-bit VA check:
    h.prv = Priv::Supervisor;
    h.pc = CODE_PA;
    h.x[10] = 0xffff_0000_8002_0000;
    h.step(&bus);
    assert_eq!(h.x[11] as u32, 0x0005_3583);

    // Non-canonical Sv57 VA (bit 56 = 0, bit 57 = 1: 0x0200_0000_8002_0000) raises LOAD_PAGE_FAULT:
    h.prv = Priv::Supervisor;
    h.pc = CODE_PA;
    h.x[10] = 0x0200_0000_8002_0000;
    h.step(&bus);
    assert_eq!(h.csr.mcause, cause::LOAD_PAGE_FAULT);
    assert_eq!(h.csr.mtval, 0x0200_0000_8002_0000);

    // 3. Zawrs (wrs.nto = 0x00d0_0073, wrs.sto = 0x01d0_0073):
    bus.write(CODE_PA + 0x10, AccessSize::Word, 0x00d0_0073)
        .unwrap(); // wrs.nto
    bus.write(CODE_PA + 0x14, AccessSize::Word, 0x01d0_0073)
        .unwrap(); // wrs.sto

    // With TW=0 in S-mode, both wrs.nto and wrs.sto retire normally:
    h.prv = Priv::Supervisor;
    h.pc = CODE_PA + 0x10;
    h.step(&bus);
    assert_eq!(h.pc, CODE_PA + 0x14);
    h.step(&bus);
    assert_eq!(h.pc, CODE_PA + 0x18);

    // With TW=1 in S-mode, wrs.sto still retires, while wrs.nto raises ILLEGAL_INSN (cause=2):
    h.csr.mstatus |= csr::status::TW;
    h.prv = Priv::Supervisor;
    h.pc = CODE_PA + 0x14;
    h.step(&bus); // wrs.sto succeeds even when TW=1
    assert_eq!(h.pc, CODE_PA + 0x18);

    h.prv = Priv::Supervisor;
    h.pc = CODE_PA + 0x10;
    h.step(&bus); // wrs.nto traps when TW=1 in S-mode
    assert_eq!(h.prv, Priv::Machine);
    assert_eq!(h.csr.mcause, cause::ILLEGAL_INSN);
    assert_eq!(h.csr.mtval, 0x00d0_0073);
}

/// PMP verdicts are cached per 4 KiB page while all PMP boundaries are
/// page-aligned. A PMP write must invalidate them, and M-mode and S/U-mode
/// verdicts must not share a cache slot.
#[test]
fn pmp_page_cache_follows_pmp_writes_and_privilege() {
    use rivemu_cpu_riscv::{Priv, cause, csr};

    const CODE: u64 = 0x8000_0000;
    const DATA: u64 = 0x8001_0000;
    const MTVEC: u64 = 0x8000_0800;
    let mut bus = AddressSpace::new();
    bus.add_ram("ram", CODE, Arc::new(Ram::new(MIB).unwrap()))
        .unwrap();
    bus.write(CODE, AccessSize::Word, 0x0005_3583).unwrap(); // ld x11, 0(x10)
    bus.write(CODE + 4, AccessSize::Word, 0x00c5_3023).unwrap(); // sd x12, 0(x10)
    bus.write(DATA, AccessSize::Double, 0x1122_3344_5566_7788)
        .unwrap();

    let mut h = Hart::new(0, Arc::new(FrozenTimebase(0)));
    h.reset(CODE);
    h.csr.mtvec = MTVEC;
    // Entry 0: NAPOT 64 KiB of code, RWX. Entry 1: NAPOT 4 KiB at DATA.
    h.csr_write(csr::PMPADDR0, (CODE | 0x7fff) >> 2).unwrap();
    h.csr_write(csr::PMPADDR0 + 1, (DATA | 0x7ff) >> 2).unwrap();
    let set_data_perms = |h: &mut Hart, rwx: u64| {
        h.prv = Priv::Machine;
        h.csr_write(csr::PMPCFG0, ((0x18 | rwx) << 8) | 0x1f)
            .unwrap();
    };
    let run_s = |h: &mut Hart, pc: u64| {
        h.prv = Priv::Supervisor;
        h.pc = pc;
        h.x[10] = DATA;
        h.x[12] = 0xabcd;
        h.step(&bus);
    };

    // R only: S-mode load succeeds (and caches the verdict), store faults.
    set_data_perms(&mut h, 0b001);
    run_s(&mut h, CODE);
    assert_eq!(h.x[11], 0x1122_3344_5566_7788);
    run_s(&mut h, CODE + 4);
    assert_eq!(h.pc, MTVEC);
    assert_eq!(h.csr_read(csr::MCAUSE), Some(cause::STORE_ACCESS));

    // RW: the cached "store denied" must not survive the PMP write.
    set_data_perms(&mut h, 0b011);
    run_s(&mut h, CODE + 4);
    assert_eq!(h.pc, CODE + 8);
    assert_eq!(bus.read(DATA, AccessSize::Double).unwrap(), 0xabcd);

    // No permissions: S-mode load faults, while M-mode (entry not locked)
    // still loads from the same page.
    set_data_perms(&mut h, 0b000);
    run_s(&mut h, CODE);
    assert_eq!(h.pc, MTVEC);
    assert_eq!(h.csr_read(csr::MCAUSE), Some(cause::LOAD_ACCESS));
    h.prv = Priv::Machine;
    h.pc = CODE;
    h.x[11] = 0;
    h.step(&bus);
    assert_eq!(h.x[11], 0xabcd);
    assert_eq!(h.pc, CODE + 4);
}

/// Zicbom/Zicboz: a cache-block operation on physical memory with no PMA
/// region raises a store/AMO access fault (Sail `phys_access_check`). Found by
/// riscv-arch-test Sv_Zicbo_*_zicbom_exceptions (cp_leaf_PTE_to_nonexistent_pa):
/// rivemu checked PMP only, so cbo.clean/flush/inval silently succeeded.
#[test]
fn cbo_to_unmapped_physical_memory_raises_store_access_fault() {
    use rivemu_cpu_riscv::{cause, csr};

    const CODE: u64 = 0x8000_0000;
    const MTVEC: u64 = 0x8000_0800;
    let mut bus = AddressSpace::new();
    bus.add_ram("ram", CODE, Arc::new(Ram::new(MIB).unwrap()))
        .unwrap();
    // cbo.clean, cbo.flush, cbo.inval, cbo.zero (a0)
    let insns: [u64; 4] = [0x0015_200f, 0x0025_200f, 0x0005_200f, 0x0045_200f];
    for (i, &w) in insns.iter().enumerate() {
        bus.write(CODE + i as u64 * 4, AccessSize::Word, w).unwrap();
    }
    let mut h = Hart::new(0, Arc::new(FrozenTimebase(0)));
    for i in 0..insns.len() as u64 {
        let pc = CODE + i * 4;
        h.reset(pc);
        h.csr.mtvec = MTVEC;
        h.x[10] = 0x123;
        h.step(&bus);
        assert_eq!(h.pc, MTVEC, "insn {i}");
        assert_eq!(
            h.csr_read(csr::MCAUSE),
            Some(cause::STORE_ACCESS),
            "insn {i}"
        );
        assert_eq!(h.csr_read(csr::MTVAL), Some(0x123), "insn {i}");

        h.reset(pc);
        h.x[10] = CODE + 0x1000;
        h.step(&bus);
        assert_eq!(h.pc, pc + 4, "insn {i}");
    }
}

/// `mcycle` and `minstret` are separate counters (privileged spec 3.1.10/3.1.12):
/// `mcountinhibit.CY` and `.IR` stop one each, Smcntrpmf filters them with
/// `mcyclecfg` and `minstretcfg`, and writing one leaves the other alone. Found by
/// riscv-arch-test Sm_mcsr_cntr (cp_inhibit_mcycle): rivemu returned `minstret`
/// for `mcycle`, so `mcycle` kept counting with CY = 1.
#[test]
fn mcycle_and_minstret_are_separate_counters() {
    use rivemu_cpu_riscv::csr;

    const CODE: u64 = 0x8000_0000;
    const CSRW_MCYCLE_X5: u64 = 0xb002_9073; // csrrw x0, mcycle, x5
    let mut bus = AddressSpace::new();
    bus.add_ram("ram", CODE, Arc::new(Ram::new(MIB).unwrap()))
        .unwrap();
    for i in 0..64 {
        bus.write(CODE + i * 4, AccessSize::Word, 0x0000_0013)
            .unwrap(); // nop
    }
    bus.write(CODE + 0x200, AccessSize::Word, CSRW_MCYCLE_X5)
        .unwrap();

    let mut h = Hart::new(0, Arc::new(FrozenTimebase(0)));
    h.reset(CODE);

    // CY = 1 stops mcycle only (single-step path).
    h.csr_write(csr::MCOUNTINHIBIT, 1).unwrap();
    for _ in 0..5 {
        h.step(&bus);
    }
    assert_eq!(h.csr_read(csr::MCYCLE), Some(0));
    assert_eq!(h.csr_read(csr::MINSTRET), Some(5));

    // IR = 1 stops minstret only (basic-block path).
    h.csr_write(csr::MCOUNTINHIBIT, 4).unwrap();
    assert_eq!(h.run(&bus, 8), 8);
    assert_eq!(h.csr_read(csr::MCYCLE), Some(8));
    assert_eq!(h.csr_read(csr::MINSTRET), Some(5));

    // Writing mcycle leaves minstret alone, and the written value is not
    // incremented by the writing instruction.
    h.csr_write(csr::MCOUNTINHIBIT, 0).unwrap();
    h.pc = CODE + 0x200;
    h.x[5] = 100;
    h.step(&bus);
    assert_eq!(h.csr_read(csr::MCYCLE), Some(100));
    assert_eq!(h.csr_read(csr::MINSTRET), Some(6));

    // Smcntrpmf: mcyclecfg.MINH stops mcycle in M-mode while minstret counts.
    h.csr_write(csr::MCYCLECFG, csr::hpmevent::MINH).unwrap();
    h.pc = CODE;
    for _ in 0..3 {
        h.step(&bus);
    }
    assert_eq!(h.csr_read(csr::MCYCLE), Some(100));
    assert_eq!(h.csr_read(csr::MINSTRET), Some(9));
}

#[test]
fn smepmp_and_smstateen_semantics() {
    use rivemu_cpu_riscv::{Priv, cause, csr};

    const CODE: u64 = 0x8000_0000;
    const DATA: u64 = 0x8001_0000;

    let mut bus = AddressSpace::new();
    bus.add_ram("ram", CODE, Arc::new(Ram::new(2 * MIB).unwrap()))
        .unwrap();
    bus.write(CODE, AccessSize::Word, 0x0005_3583).unwrap(); // ld x11, 0(x10)
    bus.write(CODE + 4, AccessSize::Word, 0x00c5_3023).unwrap(); // sd x12, 0(x10)
    bus.write(DATA, AccessSize::Double, 0xaaaa_bbbb_cccc_dddd)
        .unwrap();

    let mut h = Hart::new(0, Arc::new(FrozenTimebase(0)));
    h.reset(CODE);
    h.csr.mtvec = 0x8000_0100;

    // 1. Smstateen / Ssstateen:
    // "On reset, all writable mstateen bits are initialized by the hardware
    // to zeros", so S-mode cannot reach the gated state until M-mode enables
    // it. All of M0_MASK (SE | ENVCFG | CONTEXT | SRMCFG) is writable.
    for n in 0..4 {
        assert_eq!(h.csr_read(csr::MSTATEEN0 + n).unwrap(), 0);
    }
    h.prv = Priv::Supervisor;
    assert!(h.csr_read(csr::SENVCFG).is_none());
    assert!(h.csr_read(csr::SSTATEEN0).is_none());
    h.prv = Priv::Machine;
    h.csr_write(csr::MSTATEEN0, u64::MAX).unwrap();
    assert_eq!(h.csr_read(csr::MSTATEEN0).unwrap(), csr::stateen::M0_MASK);
    // Direct S-mode boot stands in for firmware and enables them.
    let mut booted = Hart::new(1, Arc::new(FrozenTimebase(0)));
    booted.init_smode_boot(CODE, 0);
    assert_eq!(booted.csr.mstateen[0], csr::stateen::M0_MASK);
    assert!(booted.csr_read(csr::SENVCFG).is_some());
    h.prv = Priv::Supervisor;
    assert!(h.csr_read(csr::SENVCFG).is_some());
    assert_eq!(h.csr_read(csr::SSTATEEN0).unwrap(), 0);

    // Clear mstateen0.ENVCFG in M-mode -> S-mode access to senvcfg traps (returns None):
    h.prv = Priv::Machine;
    h.csr_write(csr::MSTATEEN0, csr::stateen::SE).unwrap();
    h.prv = Priv::Supervisor;
    assert!(h.csr_read(csr::SENVCFG).is_none());
    assert!(h.csr_write(csr::SENVCFG, 0).is_none());
    assert_eq!(h.csr_read(csr::SSTATEEN0).unwrap(), 0);

    // Clear mstateen0.SE in M-mode -> S-mode access to sstateen0 traps (returns None):
    h.prv = Priv::Machine;
    h.csr_write(csr::MSTATEEN0, 0).unwrap();
    h.prv = Priv::Supervisor;
    assert!(h.csr_read(csr::SSTATEEN0).is_none());
    assert!(h.csr_write(csr::SSTATEEN0, 0).is_none());

    // Restore mstateen0:
    h.prv = Priv::Machine;
    h.csr_write(csr::MSTATEEN0, csr::stateen::M0_MASK).unwrap();

    // 2. Smepmp RLB (Rule Locking Bypass):
    // Lock PMP entry 0 as TOR [0, 0x8000_1000) with L=1, R=1, X=1 (0x8d):
    h.csr_write(csr::PMPADDR0, 0x8000_1000 >> 2).unwrap();
    h.csr_write(csr::PMPCFG0, 0x8d).unwrap();
    assert_eq!(h.csr_read(csr::PMPCFG0).unwrap() & 0xff, 0x8d);

    // Setting RLB when a locked PMP entry already exists is ignored:
    h.csr_write(csr::MSECCFG, csr::mseccfg::RLB).unwrap();
    assert_eq!(h.csr_read(csr::MSECCFG).unwrap() & csr::mseccfg::RLB, 0);

    // Reset hart, set RLB=1 before locking any entry, then lock entry 0 and modify it while RLB=1:
    h.reset(CODE);
    h.csr_write(csr::MSECCFG, csr::mseccfg::RLB).unwrap();
    assert_eq!(h.csr_read(csr::MSECCFG).unwrap(), csr::mseccfg::RLB);
    h.csr_write(csr::PMPADDR0, 0x8000_1000 >> 2).unwrap();
    h.csr_write(csr::PMPCFG0, 0x8d).unwrap(); // L=1, TOR, R=1, X=1
    assert_eq!(h.csr_read(csr::PMPCFG0).unwrap() & 0xff, 0x8d);
    // Because RLB=1, we can modify locked pmpcfg0 and even stage R=0, W=1 (shared data region):
    // Configure entry 0: TOR [0, 0x8001_0000) with L=1, R=0, W=0, X=1 (M-mode X-only = 0x8c)
    // Configure entry 1: TOR [0x8001_0000, 0x8002_0000) with L=0, R=0, W=1, X=0 (Shared R/W M, R S/U = 0x0a)
    h.csr_write(csr::PMPADDR0, DATA >> 2).unwrap();
    h.csr_write(csr::PMPADDR0 + 1, (DATA + 0x1000) >> 2)
        .unwrap();
    h.csr_write(csr::PMPCFG0, 0x0a8c).unwrap();
    assert_eq!(h.csr_read(csr::PMPCFG0).unwrap() & 0xffff, 0x0a8c);

    // Clear RLB and enable MML (sticky) and MMWP (sticky):
    h.csr_write(csr::MSECCFG, csr::mseccfg::MML | csr::mseccfg::MMWP)
        .unwrap();
    assert_eq!(
        h.csr_read(csr::MSECCFG).unwrap(),
        csr::mseccfg::MML | csr::mseccfg::MMWP
    );
    // MML and MMWP cannot be cleared once set:
    h.csr_write(csr::MSECCFG, 0).unwrap();
    assert_eq!(
        h.csr_read(csr::MSECCFG).unwrap(),
        csr::mseccfg::MML | csr::mseccfg::MMWP
    );
    // Under MML=1 and RLB=0, attempting to add a new locked executable rule in entry 2 is ignored:
    h.csr_write(csr::PMPCFG0, 0x8c_0a8c).unwrap();
    assert_eq!(h.csr_read(csr::PMPCFG0).unwrap() & 0xff_ffff, 0x00_0a8c);

    // Entry 0 (L=1, R=0, W=0, X=1) allows M-mode instruction fetch at CODE, and Entry 1
    // (L=0, R=0, W=1, X=0) allows M-mode R+W at DATA:
    h.prv = Priv::Machine;
    h.pc = CODE;
    h.x[10] = DATA;
    h.x[12] = 0x1234_5678_9abc_def0;
    h.step(&bus); // ld x11, 0(x10)
    assert_eq!(h.x[11], 0xaaaa_bbbb_cccc_dddd);
    h.step(&bus); // sd x12, 0(x10)
    assert_eq!(
        bus.read(DATA, AccessSize::Double).unwrap(),
        0x1234_5678_9abc_def0
    );

    // Unmatched M-mode access at DATA + 0x2000 is denied because MMWP=1:
    h.pc = CODE;
    h.x[10] = DATA + 0x2000;
    h.step(&bus);
    assert_eq!(h.csr.mcause, cause::LOAD_ACCESS);
    assert_eq!(h.csr.mtval, DATA + 0x2000);

    // Entry 1 (L=0, R=0, W=1, X=0) grants S-mode Read-only (no Write):
    // Configure entry 2 as unlocked S-mode X-only (`L=0, R=0, W=0, X=1` = 0x0c) at [0x8002_0000, 0x8003_0000):
    let scode = DATA + 0x1000;
    bus.write(scode, AccessSize::Word, 0x0005_3583).unwrap(); // ld x11, 0(x10)
    bus.write(scode + 4, AccessSize::Word, 0x00c5_3023).unwrap(); // sd x12, 0(x10)
    h.csr_write(csr::PMPADDR0 + 2, (scode + 0x1000) >> 2)
        .unwrap();
    h.csr_write(csr::PMPCFG0, 0x0c_0a8c).unwrap();

    h.prv = Priv::Supervisor;
    h.pc = scode;
    h.x[10] = DATA;
    h.x[11] = 0;
    h.step(&bus); // S-mode load from shared DATA succeeds!
    assert_eq!(h.x[11], 0x1234_5678_9abc_def0);
    h.step(&bus); // S-mode store to shared DATA traps with STORE_ACCESS!
    assert_eq!(h.prv, Priv::Machine);
    assert_eq!(h.csr.mcause, cause::STORE_ACCESS);
    assert_eq!(h.csr.mtval, DATA);
}

#[test]
fn scalar_crypto_zbkb_zbkc_zbkx_zknd_zkne_zknh_semantics() {
    let mut bus = AddressSpace::new();
    bus.add_ram("ram", 0x8000_0000, Arc::new(Ram::new(MIB).unwrap()))
        .unwrap();

    const CODE: u64 = 0x8000_0000;
    let r_type = |f7: u32, rs2: u32, rs1: u32, f3: u32, rd: u32, opc: u32| -> u32 {
        (f7 << 25) | (rs2 << 20) | (rs1 << 15) | (f3 << 12) | (rd << 7) | opc
    };
    let i_type = |imm12: u32, rs1: u32, f3: u32, rd: u32, opc: u32| -> u32 {
        (imm12 << 20) | (rs1 << 15) | (f3 << 12) | (rd << 7) | opc
    };

    let insns: &[u32] = &[
        // 0: pack      x12, x10, x11
        r_type(0x04, 11, 10, 4, 12, 0x33),
        // 1: packh     x13, x10, x11
        r_type(0x04, 11, 10, 7, 13, 0x33),
        // 2: packw     x14, x10, x11
        r_type(0x04, 11, 10, 4, 14, 0x3b),
        // 3: brev8     x15, x10
        i_type(0x687, 10, 5, 15, 0x13),
        // 4: xperm4    x16, x10, x11
        r_type(0x14, 11, 10, 2, 16, 0x33),
        // 5: xperm8    x17, x10, x11
        r_type(0x14, 11, 10, 4, 17, 0x33),
        // 6: aes64es   x18, x10, x11
        r_type(0x19, 11, 10, 0, 18, 0x33),
        // 7: aes64esm  x19, x10, x11
        r_type(0x1b, 11, 10, 0, 19, 0x33),
        // 8: aes64ds   x20, x18, x21   (inverse of aes64es when state halves are swapped/reconstructed)
        r_type(0x1d, 11, 10, 0, 20, 0x33),
        // 9: aes64dsm  x21, x10, x11
        r_type(0x1f, 11, 10, 0, 21, 0x33),
        // 10: aes64im  x22, x19
        i_type(0x300, 19, 1, 22, 0x13),
        // 11: aes64ks1i x23, x10, 0
        i_type(0x310, 10, 1, 23, 0x13),
        // 12: aes64ks2  x24, x23, x11
        r_type(0x3f, 11, 23, 0, 24, 0x33),
        // 13: sha256sum0 x25, x10
        i_type(0x100, 10, 1, 25, 0x13),
        // 14: sha256sig1 x26, x10
        i_type(0x103, 10, 1, 26, 0x13),
        // 15: sha512sum0 x27, x10
        i_type(0x104, 10, 1, 27, 0x13),
        // 16: sha512sig1 x28, x10
        i_type(0x107, 10, 1, 28, 0x13),
    ];
    for (i, &w) in insns.iter().enumerate() {
        bus.write(CODE + (i as u64) * 4, AccessSize::Word, u64::from(w))
            .unwrap();
    }

    let mut h = Hart::new(0, Arc::new(FrozenTimebase(0)));
    h.reset(CODE);
    h.x[10] = 0x0123_4567_89ab_cdef;
    h.x[11] = 0xfedc_ba98_7654_8001;

    for _ in 0..insns.len() {
        h.step(&bus);
    }

    assert_eq!(h.x[12], 0x7654_8001_89ab_cdef); // pack
    assert_eq!(h.x[13], 0x01ef); // packh
    assert_eq!(h.x[14], 0xffff_ffff_8001_cdef); // packw (sign-extended 32-bit)
    assert_eq!(h.x[15], 0x80c4_a2e6_91d5_b3f7); // brev8
    // aes64im(aes64esm(x10, x11)) == aes64es(x10, x11) because InvMixColumns(MixColumns(x)) == x!
    assert_eq!(h.x[22], h.x[18]);
    // sha256sum0 / sha256sig1 results are sign-extended 32-bit values:
    assert_eq!(h.x[25], (h.x[25] as i32) as i64 as u64);
    assert_eq!(h.x[26], (h.x[26] as i32) as i64 as u64);
    assert_ne!(h.x[27], 0);
    assert_ne!(h.x[28], 0);
}

#[test]
fn decoded_basic_block_cache_execution_and_mid_block_trap_accuracy() {
    use rivemu_cpu_riscv::{cause, csr};

    let mut bus = AddressSpace::new();
    bus.add_ram("ram", 0x8000_0000, Arc::new(Ram::new(MIB).unwrap()))
        .unwrap();

    const CODE: u64 = 0x8000_0000;
    const MTVEC: u64 = 0x8000_1000;

    // Basic block loop at CODE:
    //   0x00: addi x10, x10, 3       (0x0035_0513)
    //   0x04: addi x11, x11, -1      (0xfff5_8593)
    //   0x08: bne  x11, x0, -8       (0xfe05_9ce3)
    //   0x0c: wfi                    (0x1050_0073)
    let loop_insns: &[u32] = &[0x0035_0513, 0xfff5_8593, 0xfe05_9ce3, 0x1050_0073];
    for (i, &w) in loop_insns.iter().enumerate() {
        bus.write(CODE + (i as u64) * 4, AccessSize::Word, u64::from(w))
            .unwrap();
    }

    let mut h = Hart::new(0, Arc::new(FrozenTimebase(0)));
    h.reset(CODE);
    h.csr_write(csr::MTVEC, MTVEC).unwrap();
    h.x[10] = 0;
    h.x[11] = 10;

    // 10 iterations * 3 insns + 1 wfi = 31 retired instructions:
    let attempted = h.run(&bus, 100);
    assert_eq!(attempted, 31);
    assert_eq!(h.x[10], 30);
    assert_eq!(h.x[11], 0);
    assert_eq!(h.instret(), 31);
    assert!(h.wfi);

    // Patch 0x00 to `addi x10, x10, 7` (0x0075_0513) and flush icache/bbcache via fence.i:
    bus.write(CODE, AccessSize::Word, 0x0075_0513).unwrap();
    bus.write(CODE + 0x20, AccessSize::Word, 0x0000_100f)
        .unwrap(); // fence.i
    h.wfi = false;
    h.pc = CODE + 0x20;
    h.step(&bus); // executes fence.i, bumping icache_gen and invalidating bbcache
    h.pc = CODE;
    h.x[10] = 0;
    h.x[11] = 5;
    let attempted2 = h.run(&bus, 100);
    assert_eq!(attempted2, 16); // 5 * 3 + 1 wfi
    assert_eq!(h.x[10], 35);

    // Mid-block trap accuracy:
    //   0x100: addi x12, x0, 11      (0x00b0_0613)
    //   0x104: addi x13, x0, 22      (0x0160_0693)
    //   0x108: ld   x14, 0(x15)      (0x0007_b703) -> faults on unmapped x15 = 0xdead_0000
    //   0x10c: addi x12, x0, 99      (0x0630_0613) -> must not execute!
    //   MTVEC: wfi                   (0x1050_0073)
    let trap_block: &[u32] = &[0x00b0_0613, 0x0160_0693, 0x0007_b703, 0x0630_0613];
    for (i, &w) in trap_block.iter().enumerate() {
        bus.write(
            CODE + 0x100 + (i as u64) * 4,
            AccessSize::Word,
            u64::from(w),
        )
        .unwrap();
    }
    bus.write(MTVEC, AccessSize::Word, 0x1050_0073).unwrap();

    h.wfi = false;
    h.pc = CODE + 0x100;
    h.x[15] = 0xdead_0000;
    let instret_before = h.instret();
    let attempted3 = h.run(&bus, 100);
    // 2 instructions retired + 1 faulting ld + 1 wfi at MTVEC = 4 attempted, 3 retired:
    assert_eq!(attempted3, 4);
    assert_eq!(h.instret() - instret_before, 3);
    assert_eq!(h.x[12], 11);
    assert_eq!(h.x[13], 22);
    assert_eq!(h.csr.mcause, cause::LOAD_ACCESS);
    assert_eq!(h.csr.mepc, CODE + 0x108);
    assert_eq!(h.csr.mtval, 0xdead_0000);
}

#[test]
fn decoded_basic_block_cache_stops_at_noncontiguous_virtual_page_boundary() {
    use rivemu_cpu_riscv::{Priv, csr};

    let mut bus = AddressSpace::new();
    bus.add_ram("ram", 0x8000_0000, Arc::new(Ram::new(MIB).unwrap()))
        .unwrap();

    const VA_CODE: u64 = 0x4000_0000;
    const PA_PAGE0: u64 = 0x8000_0000;
    const PA_ADJACENT: u64 = 0x8000_1000;
    const PA_PAGE1: u64 = 0x8000_2000;
    const ROOT_PT: u64 = 0x8001_0000;
    const L1_PT: u64 = 0x8001_1000;
    const L0_PT: u64 = 0x8001_2000;

    // Map consecutive virtual pages to noncontiguous physical pages.
    bus.write(ROOT_PT + 8, AccessSize::Double, ((L1_PT >> 12) << 10) | 1)
        .unwrap();
    bus.write(L1_PT, AccessSize::Double, ((L0_PT >> 12) << 10) | 1)
        .unwrap();
    bus.write(L0_PT, AccessSize::Double, ((PA_PAGE0 >> 12) << 10) | 0xcf)
        .unwrap();
    bus.write(
        L0_PT + 8,
        AccessSize::Double,
        ((PA_PAGE1 >> 12) << 10) | 0xcf,
    )
    .unwrap();

    // The block starts with `addi a0, zero, 1` at the end of the first page.
    // The next virtual page contains `addi a0, a0, 2`, while the physically
    // adjacent (but unmapped) page contains `addi a0, a0, 4`. A block decoder
    // that crosses the virtual boundary by incrementing the old PA executes
    // the wrong instruction and leaves a0 == 5 instead of 3.
    bus.write(PA_PAGE0 + 0xffc, AccessSize::Word, 0x0010_0513)
        .unwrap();
    bus.write(PA_ADJACENT, AccessSize::Word, 0x0045_0513)
        .unwrap();
    bus.write(PA_PAGE1, AccessSize::Word, 0x0025_0513).unwrap();
    bus.write(PA_PAGE1 + 4, AccessSize::Word, 0x1050_0073)
        .unwrap(); // wfi

    let mut h = Hart::new(0, Arc::new(FrozenTimebase(0)));
    h.reset(VA_CODE + 0xffc);
    h.prv = Priv::Supervisor;
    h.csr_write(csr::SATP, (8u64 << 60) | (ROOT_PT >> 12))
        .unwrap();

    assert_eq!(h.run(&bus, 3), 3);
    assert_eq!(h.x[10], 3);
    assert!(h.wfi);
}

#[test]
fn zfh_zfhmin_zfbfmin_and_zfa_fp16_semantics() {
    use rivemu_cpu_riscv::csr::{self, status};

    let mut bus = AddressSpace::new();
    bus.add_ram("ram", 0x8000_0000, Arc::new(Ram::new(MIB).unwrap()))
        .unwrap();

    const CODE: u64 = 0x8000_0000;
    const DATA: u64 = 0x8000_2000;

    let insns: &[u32] = &[
        0xf410_00d3, // fli.h      f1, -1.0      (0xbc00)
        0xf41a_0153, // fli.h      f2, 2.0       (rs1=20 -> 0x4000)
        0x0420_f1d3, // fadd.h     f3, f1, f2    (1.0 = 0x3c00)
        0x0c21_7243, // fmadd.h    f4, f2, f2, f1 (3.0 = 0x4200)
        0x0045_1027, // fsh        f4, 0(x10)
        0x0005_1287, // flh        f5, 0(x10)
        0xe400_85d3, // fmv.x.h    x11, f1       (0xffff_ffff_ffff_bc00)
        0xf406_0353, // fmv.h.x    f6, x12       (NaN-boxed 0x3800 = 0.5)
        0x4022_73d3, // fcvt.s.h   f7, f4        (3.0 in fp32 = 0x4040_0000)
        0x4403_f453, // fcvt.h.s   f8, f7        (3.0 in fp16 = 0x4200)
        0x4483_f4d3, // fcvt.bf16.s f9, f7       (3.0 in bf16 = 0x4040)
        0x4064_f553, // fcvt.s.bf16 f10, f9      (3.0 in fp32 = 0x4040_0000)
        0x2c40_a5d3, // fminm.h    f11, f1, f4   (-1.0 = 0xbc00)
        0x2c40_b653, // fmaxm.h    f12, f1, f4   (3.0 = 0x4200)
        0xa440_c6d3, // fleq.h     x13, f1, f4   (1)
        0xa412_5753, // fltq.h     x14, f4, f1   (0)
        0x4083_77d3, // fround.h   f15, f6, rmm  (round(0.5, rmm) = 1.0 = 0x3c00 -> fmt=2: 0x4483_47d3)
    ];
    for (i, &w) in insns[..16].iter().enumerate() {
        bus.write(CODE + (i as u64) * 4, AccessSize::Word, u64::from(w))
            .unwrap();
    }
    // 16: fround.h f15, f6, rmm (f7=0x22, rs2=4, rs1=6, f3=4, rd=15, opc=0x53 -> 0x4443_47d3)
    bus.write(CODE + 16 * 4, AccessSize::Word, 0x4443_47d3)
        .unwrap();

    let mut h = Hart::new(0, Arc::new(FrozenTimebase(0)));
    h.reset(CODE);
    h.csr_write(csr::MSTATUS, status::FS).unwrap();
    h.x[10] = DATA;
    h.x[12] = 0x1234_5678_0000_3800; // low 16 bits = 0x3800 (0.5 in fp16)

    for _ in 0..17 {
        h.step(&bus);
    }

    const BOX16: u64 = 0xffff_ffff_ffff_0000;
    const BOX32: u64 = 0xffff_ffff_0000_0000;
    assert_eq!(h.f[1], BOX16 | 0xbc00);
    assert_eq!(h.f[2], BOX16 | 0x4000);
    assert_eq!(h.f[3], BOX16 | 0x3c00);
    assert_eq!(h.f[4], BOX16 | 0x4200);
    assert_eq!(h.f[5], BOX16 | 0x4200);
    assert_eq!(bus.read(DATA, AccessSize::Half).unwrap(), 0x4200);
    assert_eq!(h.x[11], 0xffff_ffff_ffff_bc00);
    assert_eq!(h.f[6], BOX16 | 0x3800);
    assert_eq!(h.f[7], BOX32 | 0x4040_0000);
    assert_eq!(h.f[8], BOX16 | 0x4200);
    assert_eq!(h.f[9], BOX16 | 0x4040);
    assert_eq!(h.f[10], BOX32 | 0x4040_0000);
    assert_eq!(h.f[11], BOX16 | 0xbc00);
    assert_eq!(h.f[12], BOX16 | 0x4200);
    assert_eq!(h.x[13], 1);
    assert_eq!(h.x[14], 0);
    assert_eq!(h.f[15], BOX16 | 0x3c00);

    // Improperly NaN-boxed 16-bit operand unboxes to canonical NaN (0x7e00 for f16, 0x7fc0 for bf16):
    h.f[1] = 0x0000_0000_0000_3c00; // not 16-bit NaN-boxed!
    // fadd.h f3, f1, f2 -> canonical NaN (0x7e00)
    h.pc = CODE + 2 * 4;
    h.step(&bus);
    assert_eq!(h.f[3], BOX16 | 0x7e00);
}

#[test]
fn zksed_zksh_and_zkr_seed_semantics() {
    use rivemu_cpu_riscv::csr::{self, mseccfg};
    use rivemu_cpu_riscv::{Priv, cause};

    let mut bus = AddressSpace::new();
    bus.add_ram("ram", 0x8000_0000, Arc::new(Ram::new(MIB).unwrap()))
        .unwrap();

    const CODE: u64 = 0x8000_0000;
    const MTVEC: u64 = 0x8000_1000;

    // 0: sm4ed  x12, x10, x11, 0  (f7=0x18, rs2=11, rs1=10, f3=0, rd=12, opc=0x33 -> 0x30b5_0633)
    // 1: sm4ks  x13, x10, x11, 1  (f7=0x3a, rs2=11, rs1=10, f3=0, rd=13, opc=0x33 -> 0x74b5_06b3)
    // 2: sm3p0  x14, x10          (imm12=0x108, rs1=10, f3=1, rd=14, opc=0x13 -> 0x1085_1713)
    // 3: sm3p1  x15, x10          (imm12=0x109, rs1=10, f3=1, rd=15, opc=0x13 -> 0x1095_1793)
    // 4: csrrw  x16, seed, x0     (0x0150_1873) -> legal R/W in M-mode, returns OPST=ES16 (0x8000_xxxx)
    // 5: csrrs  x17, seed, x0     (0x0150_28f3) -> read-only access to seed MUST trap with ILLEGAL_INSN!
    let insns: &[u32] = &[
        0x30b5_0633,
        0x74b5_06b3,
        0x1085_1713,
        0x1095_1793,
        0x0150_1873,
        0x0150_28f3,
    ];
    for (i, &w) in insns.iter().enumerate() {
        bus.write(CODE + (i as u64) * 4, AccessSize::Word, u64::from(w))
            .unwrap();
    }

    let mut h = Hart::new(0, Arc::new(FrozenTimebase(0)));
    h.reset(CODE);
    h.csr_write(csr::MTVEC, MTVEC).unwrap();
    h.x[10] = 0x0123_4567;
    h.x[11] = 0x89ab_cdef;

    // Step 0..4 (sm4ed, sm4ks, sm3p0, sm3p1, csrrw seed):
    for _ in 0..5 {
        h.step(&bus);
    }
    assert_eq!(h.pc, CODE + 5 * 4);
    assert_eq!(h.x[16] & 0xffff_0000, 0x8000_0000); // OPST = 0b10 (ES16)
    // sm3p0(0x0123_4567) = 0x01234567 ^ rol32(0x01234567, 9) ^ rol32(0x01234567, 17)
    let x = 0x0123_4567u32;
    let exp_sm3p0 = (x ^ x.rotate_left(9) ^ x.rotate_left(17)) as i32 as i64 as u64;
    let exp_sm3p1 = (x ^ x.rotate_left(15) ^ x.rotate_left(23)) as i32 as i64 as u64;
    assert_eq!(h.x[14], exp_sm3p0);
    assert_eq!(h.x[15], exp_sm3p1);

    // Step 5: csrrs x17, seed, x0 (read-only) traps with ILLEGAL_INSN:
    h.step(&bus);
    assert_eq!(h.pc, MTVEC);
    assert_eq!(h.csr.mcause, cause::ILLEGAL_INSN);

    // S-mode access to seed is gated by mseccfg.SSEED, and U-mode by mseccfg.USEED:
    h.prv = Priv::Supervisor;
    h.pc = CODE + 4 * 4; // csrrw x16, seed, x0
    h.step(&bus);
    assert_eq!(h.pc, MTVEC);
    assert_eq!(h.csr.mcause, cause::ILLEGAL_INSN);

    // Enable SSEED in mseccfg -> S-mode csrrw seed succeeds, U-mode still traps:
    h.prv = Priv::Machine;
    h.csr_write(csr::MSECCFG, mseccfg::SSEED).unwrap();
    h.prv = Priv::Supervisor;
    h.pc = CODE + 4 * 4;
    h.x[16] = 0;
    h.step(&bus);
    assert_eq!(h.pc, CODE + 5 * 4);
    assert_eq!(h.x[16] & 0xffff_0000, 0x8000_0000);

    h.prv = Priv::User;
    h.pc = CODE + 4 * 4;
    h.step(&bus);
    assert_eq!(h.pc, MTVEC);
    assert_eq!(h.csr.mcause, cause::ILLEGAL_INSN);

    // Enable USEED in mseccfg -> U-mode csrrw seed succeeds:
    h.prv = Priv::Machine;
    h.csr_write(csr::MSECCFG, mseccfg::SSEED | mseccfg::USEED)
        .unwrap();
    h.prv = Priv::User;
    h.pc = CODE + 4 * 4;
    h.x[16] = 0;
    h.step(&bus);
    assert_eq!(h.pc, CODE + 5 * 4);
    assert_eq!(h.x[16] & 0xffff_0000, 0x8000_0000);
}

#[test]
fn svrsw60t59b_smmpm_ssqosid_and_zicbop_semantics() {
    use rivemu_cpu_riscv::csr::{self, mseccfg, stateen};
    use rivemu_cpu_riscv::{Priv, cause};

    let mut bus = AddressSpace::new();
    bus.add_ram("ram", 0x8000_0000, Arc::new(Ram::new(MIB).unwrap()))
        .unwrap();

    const CODE: u64 = 0x8000_0000;
    const MTVEC: u64 = 0x8000_1000;
    const PT_ROOT: u64 = 0x8001_0000;
    const DATA_PA: u64 = 0x8002_0000;

    // 1. Svrsw60t59b: Sv39 1 GiB megapage at VA 0x0000_0000_8000_0000 with bits [60:59] = 0b11.
    // Non-leaf or leaf PTEs with bits [60:59] set must not raise a reserved-bit page fault.
    let pte_flags = 0xefu64; // V | R | W | X | A | D
    let pte_rsw60t59b = (3u64 << 59) | ((0x8000_0000u64 >> 12) << 10) | pte_flags;
    bus.write(PT_ROOT + 2 * 8, AccessSize::Double, pte_rsw60t59b)
        .unwrap();
    bus.write(DATA_PA, AccessSize::Double, 0x1122_3344_5566_7788)
        .unwrap();

    // 0: ld x11, 0(x10)           (0x0005_3583)
    // 1: prefetch.i 0(x10)        (0x0005_6013)
    // 2: prefetch.r 32(x10)       (0x0215_6013)
    // 3: prefetch.w 64(x10)       (0x0435_6013)
    // 4: csrrw x12, srmcfg, x13   (0x1816_9673)
    let insns: &[u32] = &[
        0x0005_3583,
        0x0005_6013,
        0x0215_6013,
        0x0435_6013,
        0x1816_9673,
    ];
    for (i, &w) in insns.iter().enumerate() {
        bus.write(CODE + (i as u64) * 4, AccessSize::Word, u64::from(w))
            .unwrap();
    }

    let mut h = Hart::new(0, Arc::new(FrozenTimebase(0)));
    h.reset(CODE);
    // Firmware enables the stateen-gated S-mode state (mstateen resets to 0).
    h.csr_write(csr::MSTATEEN0, csr::stateen::M0_MASK).unwrap();
    h.csr_write(csr::MTVEC, MTVEC).unwrap();
    h.csr_write(csr::SATP, (8u64 << 60) | (PT_ROOT >> 12))
        .unwrap();

    // Execute in S-mode with Svrsw60t59b leaf PTE + Zicbop prefetches + Ssqosid srmcfg:
    h.prv = Priv::Supervisor;
    h.x[10] = DATA_PA;
    h.x[13] = 0xffff_ffff_ffff_ffff;
    for _ in 0..5 {
        h.step(&bus);
    }
    assert_eq!(h.pc, CODE + 5 * 4);
    assert_eq!(h.x[11], 0x1122_3344_5566_7788);
    assert_eq!(h.csr_read(csr::SRMCFG), Some(csr::SRMCFG_MASK));

    // Clearing mstateen0.SRMCFG blocks S-mode access to srmcfg:
    h.prv = Priv::Machine;
    h.csr_write(csr::MSTATEEN0, stateen::M0_MASK & !stateen::SRMCFG)
        .unwrap();
    h.prv = Priv::Supervisor;
    h.pc = CODE + 4 * 4;
    h.step(&bus);
    assert_eq!(h.pc, MTVEC);
    assert_eq!(h.csr.mcause, cause::ILLEGAL_INSN);

    // 2. Smmpm: M-mode pointer masking (MPRV=0) with mseccfg.PMM = PMLEN_7 (2 << 32):
    h.prv = Priv::Machine;
    h.csr_write(csr::MSECCFG, 2u64 << mseccfg::PMM_SHIFT)
        .unwrap();
    h.pc = CODE; // ld x11, 0(x10)
    h.x[10] = (0x5au64 << 57) | DATA_PA; // non-canonical top 7 bits masked to 0 in M-mode
    h.x[11] = 0;
    h.step(&bus);
    assert_eq!(h.pc, CODE + 4);
    assert_eq!(h.x[11], 0x1122_3344_5566_7788);
}

#[test]
fn ssdbltrp_and_smdbltrp_double_trap_semantics() {
    use rivemu_cpu_riscv::csr::{self, envcfg, status};
    use rivemu_cpu_riscv::{Priv, cause};

    let mut bus = AddressSpace::new();
    bus.add_ram("ram", 0x8000_0000, Arc::new(Ram::new(MIB).unwrap()))
        .unwrap();
    const CODE: u64 = 0x8000_0000;
    const STVEC: u64 = 0x8000_1000;
    const MTVEC: u64 = 0x8000_2000;

    // CODE + 0: ecall  (0x0000_0073)
    // CODE + 4: sret   (0x1020_0073)
    // CODE + 8: mret   (0x3020_0073)
    // STVEC + 0: ebreak (0x0010_0073)
    bus.write(CODE, AccessSize::Word, 0x0000_0073).unwrap();
    bus.write(CODE + 4, AccessSize::Word, 0x1020_0073).unwrap();
    bus.write(CODE + 8, AccessSize::Word, 0x3020_0073).unwrap();
    bus.write(STVEC, AccessSize::Word, 0x0010_0073).unwrap();

    let mut h = Hart::new(0, Arc::new(FrozenTimebase(0)));
    h.smdbltrp = true;
    h.reset(CODE);
    // Smdbltrp sets MDT at reset; this test starts from MDT = 0.
    h.csr.mstatus &= !status::MDT;
    h.csr_write(csr::STVEC, STVEC).unwrap();
    h.csr_write(csr::MTVEC, MTVEC).unwrap();
    h.csr_write(
        csr::MEDELEG,
        (1 << cause::ECALL_U) | (1 << cause::BREAKPOINT),
    )
    .unwrap();

    // 1. When menvcfg.DTE == 0, SDT is read-only 0:
    h.csr_write(csr::MSTATUS, status::SDT | status::SIE)
        .unwrap();
    assert_eq!(h.csr_read(csr::MSTATUS).unwrap() & status::SDT, 0);
    assert_ne!(h.csr_read(csr::MSTATUS).unwrap() & status::SIE, 0);

    // 2. Writing mstatus.MDT = 1 automatically clears MIE:
    h.csr_write(csr::MSTATUS, status::MIE).unwrap();
    assert_ne!(h.csr_read(csr::MSTATUS).unwrap() & status::MIE, 0);
    h.csr_write(csr::MSTATUS, status::MDT | status::MIE)
        .unwrap();
    assert_eq!(
        h.csr_read(csr::MSTATUS).unwrap() & (status::MDT | status::MIE),
        status::MDT
    );

    // 3. Enabling menvcfg.DTE = 1 allows writing SDT = 1, which auto-clears SIE:
    h.csr_write(csr::MENVCFG, envcfg::DTE).unwrap();
    h.csr_write(csr::SSTATUS, status::SIE).unwrap();
    assert_ne!(h.csr_read(csr::SSTATUS).unwrap() & status::SIE, 0);
    h.csr_write(csr::SSTATUS, status::SDT | status::SIE)
        .unwrap();
    assert_eq!(
        h.csr_read(csr::SSTATUS).unwrap() & (status::SDT | status::SIE),
        status::SDT
    );

    // Clearing menvcfg.DTE = 0 clears SDT:
    h.csr_write(csr::MENVCFG, 0).unwrap();
    assert_eq!(h.csr_read(csr::MSTATUS).unwrap() & status::SDT, 0);

    // 4. S-mode trap entry with DTE=1 sets SDT=1; second S-mode trap escalates to M-mode (cause 16):
    h.csr_write(csr::MENVCFG, envcfg::DTE).unwrap();
    h.csr_write(csr::MSTATUS, 0).unwrap();
    h.prv = Priv::User;
    h.pc = CODE; // ecall -> delegated to S-mode
    h.step(&bus);
    assert_eq!(h.prv, Priv::Supervisor);
    assert_eq!(h.pc, STVEC);
    assert_eq!(h.csr.scause, cause::ECALL_U);
    assert_ne!(h.csr.mstatus & status::SDT, 0);

    // At STVEC: ebreak (delegated in medeleg) while SDT=1 -> escalates to M-mode with DOUBLE_TRAP (16)!
    h.step(&bus);
    assert_eq!(h.prv, Priv::Machine);
    assert_eq!(h.pc, MTVEC);
    assert_eq!(h.csr.mcause, cause::DOUBLE_TRAP);
    assert_eq!(h.csr.mepc, STVEC);
    assert_ne!(h.csr.mstatus & status::SDT, 0);

    // 5. MRET to S-mode (MPP=Supervisor) clears MDT, preserves SDT:
    h.csr.mstatus |= status::MDT;
    h.csr.mepc = CODE + 4; // sret
    h.pc = CODE + 8; // mret
    h.step(&bus);
    assert_eq!(h.prv, Priv::Supervisor);
    assert_eq!(h.pc, CODE + 4);
    assert_eq!(h.csr.mstatus & status::MDT, 0);
    assert_ne!(h.csr.mstatus & status::SDT, 0);

    // 6. SRET clears SDT:
    h.csr.sepc = CODE;
    h.step(&bus);
    assert_eq!(h.prv, Priv::User);
    assert_eq!(h.pc, CODE);
    assert_eq!(h.csr.mstatus & status::SDT, 0);
}

#[test]
fn sdtrig_mcontrol6_and_icount_semantics() {
    use rivemu_cpu_riscv::csr::{self, stateen, trig};
    use rivemu_cpu_riscv::{Priv, cause};

    const CODE: u64 = 0x8000_0000;
    const DATA: u64 = 0x8000_0100;
    const MTVEC: u64 = 0x8000_0200;

    let mut bus = AddressSpace::new();
    bus.add_ram("ram", 0x8000_0000, Arc::new(Ram::new(MIB).unwrap()))
        .unwrap();
    // CODE+0: addi a0, zero, 1     (0x0010_0513)
    // CODE+4: lw   a1, 0(s0)       (0x0004_2583)
    // CODE+8: sw   a0, 4(s0)       (0x00a4_2223)
    // CODE+12: addi a0, a0, 1      (0x0015_0513)
    bus.write(CODE, AccessSize::Word, 0x0010_0513).unwrap();
    bus.write(CODE + 4, AccessSize::Word, 0x0004_2583).unwrap();
    bus.write(CODE + 8, AccessSize::Word, 0x00a4_2223).unwrap();
    bus.write(CODE + 12, AccessSize::Word, 0x0015_0513).unwrap();
    bus.write(DATA, AccessSize::Word, 0x1234_5678).unwrap();

    let mut h = Hart::new(0, Arc::new(FrozenTimebase(0)));
    h.reset(CODE);
    h.csr_write(csr::MTVEC, MTVEC).unwrap();
    h.x[8] = DATA; // s0 = DATA

    // 1. tselect bounds & tinfo read:
    h.csr_write(csr::TSELECT, 3).unwrap();
    assert_eq!(h.csr_read(csr::TSELECT).unwrap(), 3);
    h.csr_write(csr::TSELECT, 4).unwrap(); // out of range (4 triggers: 0..3)
    assert_eq!(h.csr_read(csr::TSELECT).unwrap(), 3);
    assert_eq!(h.csr_read(csr::TINFO).unwrap(), trig::TINFO_VAL);

    // 2. scontext (0x5a8) gated by mstateen0.CONTEXT and mcontext (0x7a8) 13-bit mask:
    h.csr_write(csr::MSTATEEN0, 0).unwrap();
    h.prv = Priv::Supervisor;
    assert!(h.csr_read(csr::SCONTEXT).is_none());
    h.prv = Priv::Machine;
    h.csr_write(csr::MSTATEEN0, stateen::CONTEXT).unwrap();
    h.prv = Priv::Supervisor;
    h.csr_write(csr::SCONTEXT, 0xdead_beef_cafe_1234).unwrap();
    assert_eq!(h.csr_read(csr::SCONTEXT).unwrap(), 0xcafe_1234);
    h.prv = Priv::Machine;
    h.csr_write(csr::MCONTEXT, 0xffff_ffff).unwrap();
    assert_eq!(h.csr_read(csr::MCONTEXT).unwrap(), 0x1fff);

    // 3. mcontrol6 execute trigger in U-mode with tdata3 (textra64) scontext filter:
    h.csr_write(csr::TSELECT, 0).unwrap();
    h.csr_write(csr::SCONTEXT, 0x42).unwrap();
    // Require scontext == 0x99 (sselect=1, svalue=0x99 -> bits [33:2] = 0x99, bits [1:0] = 1):
    h.csr_write(csr::TDATA3, (0x99 << 2) | 1).unwrap();
    h.csr_write(csr::TDATA2, CODE).unwrap();
    h.csr_write(
        csr::TDATA1,
        (trig::TYPE_MCONTROL6 << trig::TYPE_SHIFT) | trig::U | trig::EXECUTE,
    )
    .unwrap();
    h.prv = Priv::User;
    h.pc = CODE;
    h.step(&bus);
    // Should NOT fire because scontext (0x42) != 0x99:
    assert_eq!(h.pc, CODE + 4);
    assert_eq!(h.x[10], 1);

    // Now set scontext = 0x99 and re-execute at CODE -> trigger fires!
    h.prv = Priv::Machine;
    h.csr_write(csr::SCONTEXT, 0x99).unwrap();
    h.prv = Priv::User;
    h.pc = CODE;
    h.step(&bus);
    assert_eq!(h.prv, Priv::Machine);
    assert_eq!(h.pc, MTVEC);
    assert_eq!(h.csr.mcause, cause::BREAKPOINT);
    assert_eq!(h.csr.mepc, CODE);
    assert_eq!(h.csr.mtval, CODE);

    // Disable trigger 0 and clear tdata3:
    h.csr_write(csr::TDATA1, trig::TYPE_DISABLED << trig::TYPE_SHIFT)
        .unwrap();
    h.csr_write(csr::TDATA3, 0).unwrap();

    // 4. mcontrol6 NAPOT load trigger (8-byte NAPOT region [DATA, DATA+7]):
    // NAPOT encoding for 8-byte block: low 3 bits = 0b011 (3).
    h.csr_write(csr::TDATA2, DATA | 0b011).unwrap();
    h.csr_write(
        csr::TDATA1,
        (trig::TYPE_MCONTROL6 << trig::TYPE_SHIFT)
            | trig::U
            | trig::LOAD
            | (1 << trig::MATCH_SHIFT),
    )
    .unwrap();
    h.prv = Priv::User;
    h.pc = CODE + 4; // lw a1, 0(s0)
    h.step(&bus);
    assert_eq!(h.prv, Priv::Machine);
    assert_eq!(h.csr.mcause, cause::BREAKPOINT);
    assert_eq!(h.csr.mepc, CODE + 4);
    assert_eq!(h.csr.mtval, DATA);

    // 5. Chained store range trigger [DATA+4, DATA+8) using triggers 0 (>= DATA+4, CHAIN) and 1 (< DATA+8):
    h.csr_write(csr::TSELECT, 0).unwrap();
    h.csr_write(csr::TDATA2, DATA + 4).unwrap();
    h.csr_write(
        csr::TDATA1,
        (trig::TYPE_MCONTROL6 << trig::TYPE_SHIFT)
            | trig::U
            | trig::STORE
            | trig::CHAIN
            | (2 << trig::MATCH_SHIFT), // >=
    )
    .unwrap();
    h.csr_write(csr::TSELECT, 1).unwrap();
    h.csr_write(csr::TDATA2, DATA + 8).unwrap();
    h.csr_write(
        csr::TDATA1,
        (trig::TYPE_MCONTROL6 << trig::TYPE_SHIFT)
            | trig::U
            | trig::STORE
            | (3 << trig::MATCH_SHIFT), // <
    )
    .unwrap();
    h.prv = Priv::User;
    h.pc = CODE + 8; // sw a0, 4(s0) -> stores to DATA+4
    h.step(&bus);
    assert_eq!(h.prv, Priv::Machine);
    assert_eq!(h.csr.mcause, cause::BREAKPOINT);
    assert_eq!(h.csr.mepc, CODE + 8);
    assert_eq!(h.csr.mtval, DATA + 4);

    // Disable triggers 0 and 1:
    for idx in 0..=1 {
        h.csr_write(csr::TSELECT, idx).unwrap();
        h.csr_write(csr::TDATA1, trig::TYPE_DISABLED << trig::TYPE_SHIFT)
            .unwrap();
    }

    // 6. icount trigger (type = 3, count = 1, U = 1):
    // Retiring one U-mode instruction sets PENDING; the next instruction traps before execution.
    h.csr_write(csr::TSELECT, 2).unwrap();
    h.csr_write(
        csr::TDATA1,
        (trig::TYPE_ICOUNT << trig::TYPE_SHIFT) | (1 << trig::ICOUNT_COUNT_SHIFT) | trig::ICOUNT_U,
    )
    .unwrap();
    h.prv = Priv::User;
    h.pc = CODE;
    h.x[10] = 0;
    // Step 1 retires CODE+0 (addi a0, zero, 1) and arms PENDING:
    h.step(&bus);
    assert_eq!(h.prv, Priv::User);
    assert_eq!(h.pc, CODE + 4);
    assert_eq!(h.x[10], 1);
    // Step 2 traps with BREAKPOINT before executing CODE+4:
    h.step(&bus);
    assert_eq!(h.prv, Priv::Machine);
    assert_eq!(h.pc, MTVEC);
    assert_eq!(h.csr.mcause, cause::BREAKPOINT);
    assert_eq!(h.csr.mepc, CODE + 4);
}

#[test]
fn rvv_vector_configuration_memory_alu_mask_and_fp_semantics() {
    use rivemu_cpu_riscv::{cause, csr, csr::status};

    let mut bus = AddressSpace::new();
    bus.add_ram("ram", 0x8000_0000, Arc::new(Ram::new(MIB).unwrap()))
        .unwrap();

    const CODE: u64 = 0x8000_0000;
    const DATA: u64 = 0x8000_2000;
    const MTVEC: u64 = 0x8000_4000;

    let mut h = Hart::new(0, Arc::new(FrozenTimebase(0)));
    h.reset(CODE);
    h.csr.mtvec = MTVEC;

    // 1. With mstatus.VS == Off (default after reset), executing vsetvli traps with ILLEGAL_INSN:
    // vsetvli a1, a0, e64, m1, tu, mu: zimm = 0b000000_011_000 = 0x018
    // raw = (0x018 << 20) | (10 << 15) | (7 << 12) | (11 << 7) | 0x57 = 0x0185_75d7
    let vsetvli_e64_m1 = 0x0185_75d7u32;
    bus.write(CODE, AccessSize::Word, u64::from(vsetvli_e64_m1))
        .unwrap();
    h.x[10] = 4;
    h.step(&bus);
    assert_eq!(h.pc, MTVEC);
    assert_eq!(h.csr.mcause, cause::ILLEGAL_INSN);

    // 2. Enable mstatus.VS = Initial (1 << 9) and mstatus.FS = Initial (1 << 13):
    h.csr.mstatus |= (1 << status::VS_SHIFT) | (1 << status::FS_SHIFT);
    h.pc = CODE;
    h.step(&bus);
    assert_eq!(h.pc, CODE + 4);
    assert_eq!(h.x[11], 4); // vl = 4 (VLEN=256, SEW=64, LMUL=1 -> VLMAX=4)
    assert_eq!(h.csr.vl, 4);
    assert_eq!(h.csr.vtype, 0x18);
    assert_eq!(h.csr_read(csr::VLENB).unwrap(), 32);
    assert_eq!(h.csr.mstatus & status::VS, status::VS); // VS transitioned to Dirty (3 << 9)

    // 3. Populate 4 u64 elements at DATA and DATA+32, load into v1 and v2, compute v3 = v1 + v2, store to DATA+64:
    let a_vals = [10u64, 20, 30, 40];
    let b_vals = [1u64, 2, 3, 4];
    for i in 0..4 {
        bus.write(DATA + (i as u64) * 8, AccessSize::Double, a_vals[i])
            .unwrap();
        bus.write(DATA + 32 + (i as u64) * 8, AccessSize::Double, b_vals[i])
            .unwrap();
    }
    h.x[12] = DATA;
    h.x[13] = DATA + 32;
    h.x[14] = DATA + 64;

    // vle64.v v1, (a2): nf=0, mew=0, mop=0, vm=1, lumop=0, rs1=12, width=7, vd=1, op=0x07
    let vle64_v1: u32 = (1 << 25) | (12 << 15) | (7 << 12) | (1 << 7) | 0x07;
    // vle64.v v2, (a3):
    let vle64_v2: u32 = (1 << 25) | (13 << 15) | (7 << 12) | (2 << 7) | 0x07;
    // vadd.vv v3, v2, v1 (funct6=0x00, vm=1, vs2=2, vs1=1, f3=0, vd=3, op=0x57):
    let vadd_v3_v2_v1: u32 = (1 << 25) | (2 << 20) | (1 << 15) | (3 << 7) | 0x57;
    // vse64.v v3, (a4):
    let vse64_v3: u32 = (1 << 25) | (14 << 15) | (7 << 12) | (3 << 7) | 0x27;
    // vredsum.vs v4, v3, v1 (funct6=0x00, vm=1, vs2=3, vs1=1, f3=2 (OPMVV), vd=4, op=0x57):
    let vredsum_v4: u32 = (1 << 25) | (3 << 20) | (1 << 15) | (2 << 12) | (4 << 7) | 0x57;
    // vmv.x.s a5, v4 (funct6=0x10, vm=1, vs2=4, rs1=0, f3=2 (OPMVV), rd=15, op=0x57):
    let vmv_x_s_a5_v4: u32 = (0x10 << 26) | (1 << 25) | (4 << 20) | (2 << 12) | (15 << 7) | 0x57;

    let prog = [
        vle64_v1,
        vle64_v2,
        vadd_v3_v2_v1,
        vse64_v3,
        vredsum_v4,
        vmv_x_s_a5_v4,
    ];
    for (idx, &insn) in prog.iter().enumerate() {
        bus.write(
            CODE + 4 + (idx as u64) * 4,
            AccessSize::Word,
            u64::from(insn),
        )
        .unwrap();
    }
    for _ in 0..prog.len() {
        h.step(&bus);
    }
    for i in 0..4 {
        let got = bus
            .read(DATA + 64 + (i as u64) * 8, AccessSize::Double)
            .unwrap();
        assert_eq!(got, a_vals[i] + b_vals[i]);
    }
    // vredsum.vs sums v3 = [11, 22, 33, 44] (= 110) + v1[0] (10) = 120:
    assert_eq!(h.x[15], 120);

    // 4. Mask operations: vmseq.vi v0, v2, 2 (marks element 1) + vcpop.m a6, v0 + vfirst.m a7, v0:
    // vmseq.vi v0, v2, 2: funct6=0x18, vm=1, vs2=2, simm5=2, f3=3 (OPIVI), vd=0, op=0x57
    let vmseq_v0: u32 = (0x18 << 26) | (1 << 25) | (2 << 20) | (2 << 15) | (3 << 12) | 0x57;
    // vcpop.m a6, v0: funct6=0x10, vm=1, vs2=0, rs1=16, f3=2, rd=16, op=0x57
    let vcpop_a6: u32 = (0x10 << 26) | (1 << 25) | (16 << 15) | (2 << 12) | (16 << 7) | 0x57;
    // vfirst.m a7, v0: funct6=0x10, vm=1, vs2=0, rs1=17, f3=2, rd=17, op=0x57
    let vfirst_a7: u32 = (0x10 << 26) | (1 << 25) | (17 << 15) | (2 << 12) | (17 << 7) | 0x57;
    let base_pc = h.pc;
    for (idx, &insn) in [vmseq_v0, vcpop_a6, vfirst_a7].iter().enumerate() {
        bus.write(
            base_pc + (idx as u64) * 4,
            AccessSize::Word,
            u64::from(insn),
        )
        .unwrap();
    }
    for _ in 0..3 {
        h.step(&bus);
    }
    assert_eq!(h.x[16], 1); // 1 bit set in mask
    assert_eq!(h.x[17], 1); // index 1

    // 5. Vector FP: vfadd.vv v5, v1, v2 (with f64 elements):
    let f1 = [1.5f64, 2.5, 3.5, 4.5];
    let f2 = [0.5f64, 1.5, 2.5, 3.5];
    for i in 0..4 {
        bus.write(DATA + (i as u64) * 8, AccessSize::Double, f1[i].to_bits())
            .unwrap();
        bus.write(
            DATA + 32 + (i as u64) * 8,
            AccessSize::Double,
            f2[i].to_bits(),
        )
        .unwrap();
    }
    // vfadd.vv v5, v2, v1: funct6=0x00, vm=1, vs2=2, vs1=1, f3=1 (OPFVV), vd=5, op=0x57
    let vfadd_v5 = (1 << 25) | (2 << 20) | (1 << 15) | (1 << 12) | (5 << 7) | 0x57;
    // vse64.v v5, (a4):
    let vse64_v5 = (1 << 25) | (14 << 15) | (7 << 12) | (5 << 7) | 0x27;
    let base_pc = h.pc;
    for (idx, &insn) in [vle64_v1, vle64_v2, vfadd_v5, vse64_v5].iter().enumerate() {
        bus.write(
            base_pc + (idx as u64) * 4,
            AccessSize::Word,
            u64::from(insn),
        )
        .unwrap();
    }
    for _ in 0..4 {
        h.step(&bus);
    }
    for i in 0..4 {
        let got = f64::from_bits(
            bus.read(DATA + 64 + (i as u64) * 8, AccessSize::Double)
                .unwrap(),
        );
        assert_eq!(got, f1[i] + f2[i]);
    }
}

/// RVV 1.0 7.8: field `f` of a segment access uses the register group at
/// `vd + f * ceil(EMUL)`. 3.7: a trapping vector memory access leaves the
/// faulting element index in `vstart`. Reserved register groupings raise an
/// illegal-instruction exception, as in the Sail model.
#[test]
fn rvv_segment_groups_vstart_on_fault_and_reserved_memory_encodings() {
    use rivemu_cpu_riscv::{cause, csr::status};

    // 1 MiB of RAM: 0x8000_0000..0x8010_0000; accesses beyond it fault.
    let mut bus = AddressSpace::new();
    bus.add_ram("ram", 0x8000_0000, Arc::new(Ram::new(MIB).unwrap()))
        .unwrap();
    const CODE: u64 = 0x8000_0000;
    const SRC: u64 = 0x8000_2000;
    const DST: u64 = 0x8000_3000;
    const MTVEC: u64 = 0x8000_4000;
    const RAM_END: u64 = 0x8010_0000;

    // vsetvli t0, a1, e32, m2 (vtype 0x11)
    const VSETVLI_E32_M2: u32 = (0x11 << 20) | (11 << 15) | (7 << 12) | (5 << 7) | 0x57;
    // v{l,s}seg2e32.v vreg, (a0) and vle32.v
    let vlseg2e32 = |vd: u32| (1 << 29) | (1 << 25) | (10 << 15) | (6 << 12) | (vd << 7) | 0x07;
    let vsseg2e32 = |vs3: u32| (1 << 29) | (1 << 25) | (12 << 15) | (6 << 12) | (vs3 << 7) | 0x27;
    let vle32 = |vd: u32, vm: u32| (vm << 25) | (10 << 15) | (6 << 12) | (vd << 7) | 0x07;
    let vlseg8e32 = |vd: u32| (7 << 29) | (1 << 25) | (10 << 15) | (6 << 12) | (vd << 7) | 0x07;
    let vse32_v0_masked: u32 = (12 << 15) | (6 << 12) | 0x27;

    // Each instruction gets its own slot: rewriting one address would hit
    // the decoded-instruction cache without a `fence.i`.
    let slot = std::cell::Cell::new(0u64);
    let run = |h: &mut Hart, bus: &AddressSpace, insn: u32| {
        let pc = CODE + 4 * slot.replace(slot.get() + 1);
        bus.write(pc, AccessSize::Word, u64::from(insn)).unwrap();
        h.pc = pc;
        h.step(bus);
    };
    let words = |h: &Hart, reg: usize| -> Vec<u32> {
        h.v[reg]
            .chunks(4)
            .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
            .collect()
    };

    let mut h = Hart::new(0, Arc::new(FrozenTimebase(0)));
    h.reset(CODE);
    h.csr.mtvec = MTVEC;
    h.csr.mstatus |= 1 << status::VS_SHIFT;
    h.x[11] = 16;
    run(&mut h, &bus, VSETVLI_E32_M2);
    assert_eq!(h.csr.vl, 16);

    // 1. vlseg2e32.v v4 at LMUL=2: field 0 -> v4..v5, field 1 -> v6..v7.
    for k in 0..32u64 {
        bus.write(SRC + 4 * k, AccessSize::Word, k).unwrap();
    }
    h.x[10] = SRC;
    run(&mut h, &bus, vlseg2e32(4));
    assert_eq!(h.pc, CODE + 8);
    assert_eq!(words(&h, 4), [0, 2, 4, 6, 8, 10, 12, 14]);
    assert_eq!(words(&h, 5), [16, 18, 20, 22, 24, 26, 28, 30]);
    assert_eq!(words(&h, 6), [1, 3, 5, 7, 9, 11, 13, 15]);
    assert_eq!(words(&h, 7), [17, 19, 21, 23, 25, 27, 29, 31]);

    // 2. vsseg2e32.v v4 interleaves the fields back into memory order.
    h.x[12] = DST;
    run(&mut h, &bus, vsseg2e32(4));
    assert_eq!(h.pc, CODE + 12);
    for k in 0..32u64 {
        assert_eq!(bus.read(DST + 4 * k, AccessSize::Word).unwrap(), k);
    }

    // 3. vle32.v v8 whose element 5 lies past the end of RAM: the load
    //    faults, vstart = 5, and elements 0..5 have been written.
    h.v[8] = [0; 32];
    h.v[9] = [0; 32];
    for k in 0..5u64 {
        bus.write(RAM_END - 20 + 4 * k, AccessSize::Word, 0x100 + k)
            .unwrap();
    }
    h.x[10] = RAM_END - 20;
    run(&mut h, &bus, vle32(8, 1));
    assert_eq!(h.pc, MTVEC);
    assert_eq!(h.csr.mcause, cause::LOAD_ACCESS);
    assert_eq!(h.csr.vstart, 5);
    assert_eq!(words(&h, 8)[..5], [0x100, 0x101, 0x102, 0x103, 0x104]);

    // 4. Reserved encodings raise ILLEGAL_INSN (Sail `illegal_load`):
    h.csr.vstart = 0;
    h.x[10] = SRC;
    for (what, insn) in [
        ("vd not aligned to EMUL=2", vlseg2e32(5)),
        ("nf * EMUL = 16 > 8", vlseg8e32(8)),
        ("fields run past v31", vlseg2e32(30)),
        ("masked load into v0", vle32(0, 0)),
        // v0 would be read both as the mask (EEW=1) and as EEW=32 data.
        ("masked store of v0", vse32_v0_masked),
    ] {
        run(&mut h, &bus, insn);
        assert_eq!(h.pc, MTVEC, "{what}");
        assert_eq!(h.csr.mcause, cause::ILLEGAL_INSN, "{what}");
    }

    // 5. `vsetvli x0, x0, e32, m1` keeps vl only if SEW/LMUL is unchanged.
    //    From e32/m2 the ratio changes: reserved, so vill is set and vl = 0.
    const VSETVLI_X0_X0_E32_M1: u32 = (0x10 << 20) | (7 << 12) | 0x57;
    const VSETVLI_X0_X0_E16_M1: u32 = (0x08 << 20) | (7 << 12) | 0x57;
    run(&mut h, &bus, VSETVLI_E32_M2);
    run(&mut h, &bus, VSETVLI_X0_X0_E32_M1);
    assert_eq!(h.csr.vtype, 1 << 63);
    assert_eq!(h.csr.vl, 0);
    // From e32/m2 to e16/m1 the ratio is unchanged: vl is kept.
    run(&mut h, &bus, VSETVLI_E32_M2);
    run(&mut h, &bus, VSETVLI_X0_X0_E16_M1);
    assert_eq!(h.csr.vtype, 0x08);
    assert_eq!(h.csr.vl, 16);
}

/// RVV 1.0 11.11: signed division overflow (most-negative / -1) returns the
/// dividend for `vdiv` and 0 for `vrem`. At SEW = 64 the most-negative value
/// is i64::MIN, which a naive negation overflows (it panicked in debug
/// builds).
#[test]
fn rvv_signed_division_overflow_at_sew64() {
    use rivemu_cpu_riscv::csr::status;

    let mut bus = AddressSpace::new();
    bus.add_ram("ram", 0x8000_0000, Arc::new(Ram::new(MIB).unwrap()))
        .unwrap();
    const CODE: u64 = 0x8000_0000;
    // vsetvli t0, a1, e64, m1 (vtype 0x18); vdiv.vv v3, v1, v2; vrem.vv v4, v1, v2
    let prog: [u32; 3] = [
        (0x18 << 20) | (11 << 15) | (7 << 12) | (5 << 7) | 0x57,
        (0x21 << 26) | (1 << 25) | (1 << 20) | (2 << 15) | (2 << 12) | (3 << 7) | 0x57,
        (0x23 << 26) | (1 << 25) | (1 << 20) | (2 << 15) | (2 << 12) | (4 << 7) | 0x57,
    ];
    for (i, insn) in prog.iter().enumerate() {
        bus.write(CODE + 4 * i as u64, AccessSize::Word, u64::from(*insn))
            .unwrap();
    }
    let mut h = Hart::new(0, Arc::new(FrozenTimebase(0)));
    h.reset(CODE);
    h.csr.mstatus |= 1 << status::VS_SHIFT;
    h.x[11] = 4;
    for lane in 0..4 {
        h.v[1][lane * 8..lane * 8 + 8].copy_from_slice(&i64::MIN.to_le_bytes());
        h.v[2][lane * 8..lane * 8 + 8].copy_from_slice(&(-1i64).to_le_bytes());
    }
    for _ in 0..3 {
        h.step(&bus);
    }
    assert_eq!(h.pc, CODE + 12);
    for lane in 0..4 {
        let q = i64::from_le_bytes(h.v[3][lane * 8..lane * 8 + 8].try_into().unwrap());
        let r = i64::from_le_bytes(h.v[4][lane * 8..lane * 8 + 8].try_into().unwrap());
        assert_eq!((q, r), (i64::MIN, 0), "lane {lane}");
    }
}

/// Privileged spec (Svadu): with `menvcfg.ADUE = 0` "the implementation
/// behaves as though Svadu were not implemented", so a missing A bit (or a
/// missing D bit on a store) raises a page fault. A default hart resets with
/// ADUE = 1 and updates A/D in hardware; clearing ADUE must switch it to
/// Svade behaviour (Sail 0.16 `hw_updates_pte`).
#[test]
fn menvcfg_adue_selects_hardware_ad_updates_or_page_faults() {
    use rivemu_cpu_riscv::csr::envcfg;
    use rivemu_cpu_riscv::{Priv, cause};

    let mut bus = AddressSpace::new();
    bus.add_ram("ram", 0x8000_0000, Arc::new(Ram::new(MIB).unwrap()))
        .unwrap();
    // Sv39 1 GiB leaf (VA 0x8000_0000 -> PA 0x8000_0000), V|R|W|X, A = D = 0.
    let root_pt = 0x8001_0000u64;
    let pte_addr = root_pt + 2 * 8;
    let pte_base = (0x80000u64 << 10) | 0x0f;
    bus.write(pte_addr, AccessSize::Double, pte_base).unwrap();
    // lw x1, 0(x10)
    bus.write(0x8000_0000, AccessSize::Word, 0x0005_2083)
        .unwrap();

    let mut h = Hart::new(0, Arc::new(FrozenTimebase(0)));
    h.reset(0x8000_0000);
    assert_ne!(h.csr.menvcfg & envcfg::ADUE, 0, "ADUE = 1 at reset");
    let enter_s = |h: &mut Hart| {
        h.prv = Priv::Supervisor;
        h.csr.satp = (8u64 << 60) | (root_pt >> 12);
        h.csr.medeleg = 0xffff;
        h.csr.stvec = 0x8000_0200;
        h.x[10] = 0x8000_2000;
        h.pc = 0x8000_0000;
        h.invalidate_tlb();
    };

    // ADUE = 1: the fetch and load succeed and hardware sets A.
    enter_s(&mut h);
    h.step(&bus);
    assert_eq!(h.pc, 0x8000_0004);
    assert_ne!(
        bus.read(pte_addr, AccessSize::Double).unwrap() & (1 << 6),
        0
    );

    // ADUE = 0: the same access with A = 0 raises an instruction page fault.
    bus.write(pte_addr, AccessSize::Double, pte_base).unwrap();
    h.csr.menvcfg &= !envcfg::ADUE;
    enter_s(&mut h);
    h.step(&bus);
    assert_eq!(h.csr.scause, cause::INSN_PAGE_FAULT);
    assert_eq!(bus.read(pte_addr, AccessSize::Double).unwrap(), pte_base);

    // Selecting Svade (strict mode) makes ADUE = 0 the reset value.
    h.set_svade(true);
    h.reset(0x8000_0000);
    assert_eq!(h.csr.menvcfg & envcfg::ADUE, 0);
}

/// The RISC-V Semihosting sequence `slli x0, x0, 0x1f; ebreak; srai x0, x0, 7`
/// is serviced only when semihosting is enabled (`-semihosting`). Otherwise
/// its `ebreak` raises a breakpoint exception, as on QEMU without
/// `-semihosting`: OpenSBI probes for semihosting this way and switches its
/// console away from the UART when the probe does not trap.
#[test]
fn semihosting_sequence_traps_unless_enabled() {
    use rivemu_cpu_riscv::cause;

    let mut bus = AddressSpace::new();
    bus.add_ram("ram", 0x8000_0000, Arc::new(Ram::new(MIB).unwrap()))
        .unwrap();
    for (i, w) in [0x01f0_1013u64, 0x0010_0073, 0x4070_5013]
        .into_iter()
        .enumerate()
    {
        bus.write(0x8000_0000 + 4 * i as u64, AccessSize::Word, w)
            .unwrap();
    }
    for enabled in [false, true] {
        let mut h = Hart::new(0, Arc::new(FrozenTimebase(0)));
        h.reset(0x8000_0000);
        h.csr.mtvec = 0x8000_0100;
        h.semihosting = enabled;
        h.step(&bus);
        h.step(&bus);
        if enabled {
            assert!(h.pending_semihost);
            assert_eq!(h.csr.mcause, 0);
        } else {
            assert!(!h.pending_semihost);
            assert_eq!(h.csr.mcause, cause::BREAKPOINT);
            assert_eq!(h.csr.mepc, 0x8000_0004);
            assert_eq!(h.pc, 0x8000_0100);
        }
    }
}

/// Smdbltrp (`-cpu rv64,smdbltrp=on`): MDT is 1 at reset, taking a trap into
/// M-mode sets it, MRET clears it, and a trap into M-mode while MDT = 1 is an
/// unexpected trap that stops the hart without updating architectural state
/// (Sail 0.16 `machine_trap_set_mdt`). Without the option MDT is read-only 0.
#[test]
fn smdbltrp_reset_value_trap_entry_and_critical_error() {
    use rivemu_cpu_riscv::csr::{self, status};

    let mut bus = AddressSpace::new();
    bus.add_ram("ram", 0x8000_0000, Arc::new(Ram::new(MIB).unwrap()))
        .unwrap();
    // 0x00: ecall; 0x100 (mtvec): ecall again, without clearing MDT.
    bus.write(0x8000_0000, AccessSize::Word, 0x0000_0073)
        .unwrap();
    bus.write(0x8000_0100, AccessSize::Word, 0x0000_0073)
        .unwrap();

    // Default hart: MDT is read-only zero and traps do not set it.
    let mut h = Hart::new(0, Arc::new(FrozenTimebase(0)));
    h.reset(0x8000_0000);
    assert_eq!(h.csr.mstatus & status::MDT, 0);
    h.csr_write(csr::MSTATUS, h.csr.mstatus | status::MDT)
        .unwrap();
    assert_eq!(h.csr.mstatus & status::MDT, 0, "MDT is read-only 0");
    h.csr.mtvec = 0x8000_0100;
    h.step(&bus);
    assert_eq!(h.pc, 0x8000_0100);
    h.step(&bus);
    assert_eq!(h.pc, 0x8000_0100, "nested trap is delivered normally");
    assert!(!h.critical_error);

    // Smdbltrp hart: MDT = 1 at reset, so the first M-mode trap is unexpected.
    let mut h = Hart::new(0, Arc::new(FrozenTimebase(0)));
    h.smdbltrp = true;
    h.reset(0x8000_0000);
    assert_ne!(h.csr.mstatus & status::MDT, 0, "MDT = 1 at reset");
    h.csr.mtvec = 0x8000_0100;
    h.step(&bus);
    assert!(h.critical_error);
    assert_eq!(h.pc, 0x8000_0000, "no architectural state is updated");
    assert_eq!(h.csr.mcause, 0);

    // After software clears MDT, a trap sets it and the next trap is unexpected.
    let mut h = Hart::new(0, Arc::new(FrozenTimebase(0)));
    h.smdbltrp = true;
    h.reset(0x8000_0000);
    h.csr.mtvec = 0x8000_0100;
    h.csr_write(csr::MSTATUS, h.csr.mstatus & !status::MDT)
        .unwrap();
    h.step(&bus);
    assert_eq!(h.pc, 0x8000_0100);
    assert_ne!(h.csr.mstatus & status::MDT, 0, "trap entry sets MDT");
    h.step(&bus);
    assert!(h.critical_error);
    assert_eq!(h.pc, 0x8000_0100);
}
