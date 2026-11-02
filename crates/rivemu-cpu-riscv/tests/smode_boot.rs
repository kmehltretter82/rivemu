use std::sync::Arc;

use rivemu_core::{AccessSize, FrozenTimebase};
use rivemu_cpu_riscv::{Hart, Priv, csr};
use rivemu_mem::{AddressSpace, Ram};

const PC: u64 = 0x8000_0000;

#[test]
fn smode_boot_allows_seed_csr_when_zkr_is_advertised() {
    let mut bus = AddressSpace::new();
    bus.add_ram("ram", PC, Arc::new(Ram::new(4096).unwrap()))
        .unwrap();
    // csrrwi a0, seed, 0
    bus.write(PC, AccessSize::Word, 0x0150_5573).unwrap();

    let mut h = Hart::new(0, Arc::new(FrozenTimebase(0)));
    h.init_smode_boot(PC, 0);
    assert_eq!(h.csr.mseccfg & csr::mseccfg::SSEED, csr::mseccfg::SSEED);

    h.step(&bus);
    assert_eq!(h.prv, Priv::Supervisor);
    assert_eq!(h.pc, PC + 4);
    assert_eq!(h.x[10] & 0xffff_0000, 0x8000_0000);
}
