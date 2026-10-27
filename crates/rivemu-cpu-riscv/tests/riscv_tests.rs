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
