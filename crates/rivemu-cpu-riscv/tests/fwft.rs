use std::sync::Arc;

use rivemu_core::{AccessSize, FrozenTimebase};
use rivemu_cpu_riscv::{Hart, Priv, csr::envcfg};
use rivemu_mem::{AddressSpace, Ram};

const PC: u64 = 0x8000_0000;
const FWFT: u64 = 0x4657_4654;

fn invoke(
    h: &mut Hart,
    bus: &AddressSpace,
    fid: u64,
    feature: u64,
    value: u64,
    flags: u64,
) -> (i64, u64) {
    h.pc = PC;
    h.x[17] = FWFT;
    h.x[16] = fid;
    h.x[10] = feature;
    h.x[11] = value;
    h.x[12] = flags;
    h.step(bus);
    assert_eq!(h.prv, Priv::Supervisor);
    (h.x[10] as i64, h.x[11])
}

#[test]
fn firmware_features_enforce_values_locks_and_reset() {
    let mut bus = AddressSpace::new();
    bus.add_ram("ram", PC, Arc::new(Ram::new(4096).unwrap()))
        .unwrap();
    bus.write(PC, AccessSize::Word, 0x73).unwrap();
    let mut h = Hart::new(0, Arc::new(FrozenTimebase(0)));
    h.init_smode_boot(PC, 0);
    assert_eq!(h.csr.menvcfg & (envcfg::LPE | envcfg::SSE), 0);
    for feature in [1, 2] {
        assert_eq!(invoke(&mut h, &bus, 1, feature, 0, 0), (0, 0));
        assert_eq!(invoke(&mut h, &bus, 0, feature, 2, 0), (-3, 0));
        assert_eq!(invoke(&mut h, &bus, 0, feature, 1, 2), (-3, 0));
        assert_eq!(invoke(&mut h, &bus, 1, feature, 0, 0), (0, 0));
        assert_eq!(invoke(&mut h, &bus, 0, feature, 1, 1), (0, 0));
        assert_eq!(invoke(&mut h, &bus, 0, feature, 1, 0), (0, 0));
        assert_eq!(invoke(&mut h, &bus, 0, feature, 0, 0), (-14, 0));
        assert_eq!(invoke(&mut h, &bus, 1, feature, 0, 0), (0, 1));
    }
    assert_eq!(invoke(&mut h, &bus, 1, 4, 0, 0), (-2, 0));
    assert_eq!(invoke(&mut h, &bus, 1, 6, 0, 0), (-4, 0));
    h.init_smode_boot(PC, 0);
    assert_eq!(invoke(&mut h, &bus, 1, 1, 0, 0), (0, 0));
    assert_eq!(invoke(&mut h, &bus, 0, 1, 1, 0), (0, 0));
}
