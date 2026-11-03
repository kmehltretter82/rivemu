//! H extension: behavior the privileged spec defines where QEMU 11.1.3 (the
//! lockstep reference, see rivemu-lockstep `firmware/h_test.S`) differs, and
//! the `henvcfg`/`hstatus` controls of VS-mode and HLV/HSV.

use std::sync::Arc;

use rivemu_core::{AccessSize, FrozenTimebase, size::MIB};
use rivemu_cpu_riscv::csr::{self, envcfg, hstatus, status};
use rivemu_cpu_riscv::{Hart, Priv, cause};
use rivemu_mem::{AddressSpace, Ram};

const CODE: u64 = 0x8000_0000;
const DATA: u64 = 0x8003_0000;
/// An all-zero Sv39x4 root: every guest physical address faults.
const EMPTY_G_ROOT: u64 = 0x8001_0000;

const A0: u32 = 10;
const T0: u32 = 5;

const MPV: u64 = 1 << 39;
const GVA: u64 = 1 << 38;
const MPP_S: u64 = 1 << 11;

fn csrr(rd: u32, csr: u16) -> u32 {
    (u32::from(csr) << 20) | (2 << 12) | (rd << 7) | 0x73
}
fn csrw(csr: u16, rs1: u32) -> u32 {
    (u32::from(csr) << 20) | (rs1 << 15) | (1 << 12) | 0x73
}
fn csrrw(rd: u32, csr: u16, rs1: u32) -> u32 {
    (u32::from(csr) << 20) | (rs1 << 15) | (1 << 12) | (rd << 7) | 0x73
}
fn ld(rd: u32, rs1: u32) -> u32 {
    (rs1 << 15) | (3 << 12) | (rd << 7) | 0x03
}
fn hlv_d(rd: u32, rs1: u32) -> u32 {
    (0x36 << 25) | (rs1 << 15) | (4 << 12) | (rd << 7) | 0x73
}
fn cbo_zero(rs1: u32) -> u32 {
    (4 << 20) | (rs1 << 15) | (2 << 12) | 0x0f
}
const MRET: u32 = 0x3020_0073;
const SFENCE_VMA: u32 = 0x1200_0073;
const WRS_NTO: u32 = 0x00d0_0073;
const VAL: u64 = 0x1122_3344_5566_7788;

/// A hart with H enabled, at `CODE` in M-mode, with `insns` at `CODE` and
/// `VAL` at `DATA`.
fn hart(insns: &[u32]) -> (AddressSpace, Hart) {
    let mut bus = AddressSpace::new();
    bus.add_ram("ram", 0x8000_0000, Arc::new(Ram::new(MIB).unwrap()))
        .unwrap();
    for (i, w) in insns.iter().enumerate() {
        bus.write(CODE + 4 * i as u64, AccessSize::Word, u64::from(*w))
            .unwrap();
    }
    bus.write(DATA, AccessSize::Double, VAL).unwrap();
    let mut h = Hart::new(0, Arc::new(FrozenTimebase(0)));
    h.set_hypervisor(true);
    h.reset(CODE);
    h.csr_write(csr::MTVEC, 0x8000_8000).unwrap();
    (bus, h)
}

/// Enters `prv` with V = `virt` at `pc` through `mret` (mstatus.MPV and
/// MPP written by software), with an `mret` placed just before `pc`.
fn enter(h: &mut Hart, bus: &AddressSpace, prv: Priv, virt: bool, pc: u64) {
    bus.write(pc - 4, AccessSize::Word, u64::from(MRET))
        .unwrap();
    let mpp = match prv {
        Priv::User => 0,
        Priv::Supervisor => MPP_S,
        Priv::Machine => 3 << 11,
    };
    let s = h.csr_read(csr::MSTATUS).unwrap() & !(MPV | (3 << 11));
    h.csr_write(csr::MSTATUS, s | mpp | if virt { MPV } else { 0 })
        .unwrap();
    h.csr_write(csr::MEPC, pc).unwrap();
    h.prv = Priv::Machine;
    h.pc = pc - 4;
    h.step(bus);
    assert_eq!((h.prv, h.virt, h.pc), (prv, virt, pc));
}

#[test]
fn mpv_is_writable_and_mret_enters_vs_mode() {
    let (bus, mut h) = hart(&[]);
    h.csr_write(csr::MSTATUS, MPV | MPP_S).unwrap();
    assert_ne!(h.csr_read(csr::MSTATUS).unwrap() & MPV, 0);
    h.csr_write(csr::VSSCRATCH, 0x1234).unwrap();
    // VS-mode sees vsscratch as sscratch; mret cleared MPV.
    bus.write(
        CODE + 0x104,
        AccessSize::Word,
        u64::from(csrr(A0, csr::SSCRATCH)),
    )
    .unwrap();
    enter(&mut h, &bus, Priv::Supervisor, true, CODE + 0x104);
    assert_eq!(h.csr.mstatus & MPV, 0);
    h.step(&bus);
    assert_eq!(h.x[A0 as usize], 0x1234);
}

#[test]
fn mstatus_gva_is_set_exactly_when_mtval_holds_a_guest_address() {
    // A virtual-instruction exception from VS-mode: mtval holds the
    // instruction, so GVA = 0 (QEMU 11.1.3 sets it).
    let insn = csrr(A0, csr::HSTATUS);
    let (bus, mut h) = hart(&[]);
    bus.write(CODE + 0x104, AccessSize::Word, u64::from(insn))
        .unwrap();
    h.csr.mstatus |= GVA;
    enter(&mut h, &bus, Priv::Supervisor, true, CODE + 0x104);
    h.step(&bus);
    assert_eq!(h.csr.mcause, cause::VIRTUAL_INSN);
    assert_eq!(h.csr.mtval, u64::from(insn));
    assert_eq!(h.csr.mstatus & (MPV | GVA), MPV);

    // HLV from HS-mode (V = 0) faulting into M-mode: mtval is a guest
    // virtual address, so GVA = 1 although MPV = 0 (QEMU leaves GVA alone).
    let (bus, mut h) = hart(&[]);
    h.csr_write(csr::HGATP, (8 << 60) | (EMPTY_G_ROOT >> 12))
        .unwrap();
    bus.write(CODE + 0x104, AccessSize::Word, u64::from(hlv_d(A0, T0)))
        .unwrap();
    enter(&mut h, &bus, Priv::Supervisor, false, CODE + 0x104);
    h.x[T0 as usize] = 0x1000;
    h.step(&bus);
    assert_eq!(h.csr.mcause, cause::LOAD_GUEST_PAGE_FAULT);
    assert_eq!(h.csr.mtval, 0x1000);
    assert_eq!(h.csr_read(csr::MTVAL2).unwrap(), 0x1000 >> 2);
    assert_eq!(h.csr.mstatus & (MPV | GVA), GVA);

    // M-mode with MPRV, MPP = S and MPV = 1: the same.
    let (bus, mut h) = hart(&[ld(A0, T0)]);
    h.csr_write(csr::HGATP, (8 << 60) | (EMPTY_G_ROOT >> 12))
        .unwrap();
    h.csr_write(csr::MSTATUS, status::MPRV | MPV | MPP_S)
        .unwrap();
    h.x[T0 as usize] = 0x2008;
    h.step(&bus);
    assert_eq!(h.csr.mcause, cause::LOAD_GUEST_PAGE_FAULT);
    assert_eq!(h.csr.mtval, 0x2008);
    assert_eq!(h.csr_read(csr::MTVAL2).unwrap(), 0x2008 >> 2);
    assert_eq!(h.csr.mstatus & (MPV | GVA), GVA);
}

#[test]
fn traps_into_vs_mode_leave_hstatus_alone() {
    let (bus, mut h) = hart(&[]);
    h.csr_write(csr::MEDELEG, 1 << cause::ECALL_U).unwrap();
    h.csr_write(csr::HEDELEG, 1 << cause::ECALL_U).unwrap();
    h.csr_write(csr::VSTVEC, CODE + 0x800).unwrap();
    h.csr_write(csr::HSTATUS, hstatus::GVA | hstatus::SPVP)
        .unwrap();
    bus.write(CODE + 0x104, AccessSize::Word, 0x73).unwrap(); // ecall
    enter(&mut h, &bus, Priv::User, true, CODE + 0x104);
    h.step(&bus);
    assert_eq!(
        (h.prv, h.virt, h.pc),
        (Priv::Supervisor, true, CODE + 0x800)
    );
    assert_eq!(h.csr.scause, cause::ECALL_U);
    let hs = h.csr.h.hstatus;
    assert_eq!(
        hs & (hstatus::GVA | hstatus::SPVP),
        hstatus::GVA | hstatus::SPVP
    );
}

#[test]
fn warl_fields_of_the_h_csrs() {
    let (_bus, mut h) = hart(&[]);
    // vsepc[0] is always zero (IALIGN = 16).
    h.csr_write(csr::VSEPC, 0x1235).unwrap();
    assert_eq!(h.csr_read(csr::VSEPC).unwrap(), 0x1234);
    // Reserved hstatus bits read as zero; VSXL reads 2.
    h.csr_write(csr::HSTATUS, u64::MAX).unwrap();
    assert_eq!(
        h.csr_read(csr::HSTATUS).unwrap(),
        hstatus::WRITABLE | hstatus::VSXL_64
    );
    // HUPMM = 1 is reserved: the old value stays.
    h.csr_write(csr::HSTATUS, 2 << hstatus::HUPMM_SHIFT)
        .unwrap();
    h.csr_write(csr::HSTATUS, 1 << hstatus::HUPMM_SHIFT)
        .unwrap();
    assert_eq!(
        h.csr_read(csr::HSTATUS).unwrap() & hstatus::HUPMM_MASK,
        2 << hstatus::HUPMM_SHIFT
    );
    // hgatp: bits 59:58 reserved, PPN[1:0] zero, VMID 14 bits.
    h.csr_write(csr::HGATP, (8 << 60) | (0xffff << 44) | 0x8_0007)
        .unwrap();
    assert_eq!(
        h.csr_read(csr::HGATP).unwrap(),
        (8 << 60) | (0x3fff << 44) | 0x8_0004
    );
    // An unsupported MODE keeps the old one; VMID and PPN are written.
    h.csr_write(csr::HGATP, (5 << 60) | (0x12 << 44) | 0x9_0000)
        .unwrap();
    assert_eq!(
        h.csr_read(csr::HGATP).unwrap(),
        (8 << 60) | (0x12 << 44) | 0x9_0000
    );
    // hvip holds the VS-level bits only.
    h.csr_write(csr::MIP, 1 << 5).unwrap();
    h.csr_write(csr::HVIP, u64::MAX).unwrap();
    assert_eq!(h.csr_read(csr::HVIP).unwrap(), 0x444);
    // henvcfg.SSE needs menvcfg.SSE.
    h.csr_write(csr::MENVCFG, 0).unwrap();
    h.csr_write(csr::HENVCFG, envcfg::SSE).unwrap();
    assert_eq!(h.csr_read(csr::HENVCFG).unwrap() & envcfg::SSE, 0);
}

#[test]
fn vs_sie_does_not_depend_on_mideleg() {
    let (bus, mut h) = hart(&[]);
    h.csr_write(csr::MIDELEG, 0).unwrap();
    h.csr_write(csr::HIDELEG, 0x444).unwrap();
    bus.write(
        CODE + 0x104,
        AccessSize::Word,
        u64::from(csrw(csr::SIE, T0)),
    )
    .unwrap();
    bus.write(
        CODE + 0x108,
        AccessSize::Word,
        u64::from(csrr(A0, csr::SIE)),
    )
    .unwrap();
    enter(&mut h, &bus, Priv::Supervisor, true, CODE + 0x104);
    h.x[T0 as usize] = 0x222;
    h.step(&bus);
    h.step(&bus);
    assert_eq!(h.x[A0 as usize], 0x222);
    assert_eq!(h.csr.mie & 0x444, 0x444);
}

#[test]
fn tvm_governs_hs_mode_and_vtvm_vs_mode() {
    let (bus, mut h) = hart(&[]);
    h.csr_write(csr::MSTATUS, status::TVM).unwrap();
    bus.write(
        CODE + 0x104,
        AccessSize::Word,
        u64::from(csrr(A0, csr::SATP)),
    )
    .unwrap();
    bus.write(CODE + 0x108, AccessSize::Word, u64::from(SFENCE_VMA))
        .unwrap();
    bus.write(CODE + 0x10c, AccessSize::Word, 0x73).unwrap(); // ecall
    enter(&mut h, &bus, Priv::Supervisor, true, CODE + 0x104);
    h.step(&bus);
    h.step(&bus);
    assert_eq!(
        h.pc,
        CODE + 0x10c,
        "satp and sfence.vma in VS-mode ignore TVM"
    );
    h.step(&bus);
    assert_eq!(h.csr.mcause, cause::ECALL_VS);
    // From VU-mode, sfence.vma is a virtual instruction.
    h.csr_write(csr::MSTATUS, 0).unwrap();
    bus.write(CODE + 0x204, AccessSize::Word, u64::from(SFENCE_VMA))
        .unwrap();
    enter(&mut h, &bus, Priv::User, true, CODE + 0x204);
    h.step(&bus);
    assert_eq!(h.csr.mcause, cause::VIRTUAL_INSN);
}

#[test]
fn henvcfg_controls_cache_block_operations_in_vs_mode() {
    let (bus, mut h) = hart(&[]);
    h.csr_write(csr::MENVCFG, envcfg::CBZE).unwrap();
    h.csr_write(csr::HENVCFG, 0).unwrap();
    bus.write(CODE + 0x104, AccessSize::Word, u64::from(cbo_zero(T0)))
        .unwrap();
    enter(&mut h, &bus, Priv::Supervisor, true, CODE + 0x104);
    h.x[T0 as usize] = DATA + 8;
    h.step(&bus);
    assert_eq!(h.csr.mcause, cause::VIRTUAL_INSN);
    assert_eq!(bus.read(DATA, AccessSize::Double).unwrap(), VAL);

    h.csr_write(csr::HENVCFG, envcfg::CBZE).unwrap();
    bus.write(CODE + 0x108, AccessSize::Word, 0x73).unwrap(); // ecall
    enter(&mut h, &bus, Priv::Supervisor, true, CODE + 0x104);
    h.x[T0 as usize] = DATA + 8;
    h.step(&bus);
    assert_eq!(h.pc, CODE + 0x108);
    assert_eq!(bus.read(DATA, AccessSize::Double).unwrap(), 0);
    h.step(&bus);
    assert_eq!(h.csr.mcause, cause::ECALL_VS);

    // A guest-page fault on cbo.zero reports the guest physical address.
    // The G-stage maps GPA 2-3 GiB (the code) and nothing below.
    let leaf = ((0x8000_0000u64 >> 12) << 10) | 0xdf; // V R W X U A D
    bus.write(EMPTY_G_ROOT + 2 * 8, AccessSize::Double, leaf)
        .unwrap();
    h.csr_write(csr::HGATP, (8 << 60) | (EMPTY_G_ROOT >> 12))
        .unwrap();
    enter(&mut h, &bus, Priv::Supervisor, true, CODE + 0x104);
    h.x[T0 as usize] = 0x3008;
    h.step(&bus);
    assert_eq!(h.csr.mcause, cause::STORE_GUEST_PAGE_FAULT);
    assert_eq!(h.csr.mtval, 0x3008);
    assert_eq!(h.csr_read(csr::MTVAL2).unwrap(), 0x3000 >> 2);
}

#[test]
fn seed_and_wrs_nto_from_vs_mode() {
    let (bus, mut h) = hart(&[]);
    bus.write(
        CODE + 0x104,
        AccessSize::Word,
        u64::from(csrrw(A0, csr::SEED, 0)),
    )
    .unwrap();
    h.csr_write(csr::MSECCFG, csr::mseccfg::SSEED).unwrap();
    enter(&mut h, &bus, Priv::Supervisor, true, CODE + 0x104);
    h.step(&bus);
    assert_eq!(h.csr.mcause, cause::VIRTUAL_INSN);
    h.csr_write(csr::MSECCFG, 0).unwrap();
    enter(&mut h, &bus, Priv::Supervisor, true, CODE + 0x104);
    h.step(&bus);
    assert_eq!(h.csr.mcause, cause::ILLEGAL_INSN);

    bus.write(CODE + 0x204, AccessSize::Word, u64::from(WRS_NTO))
        .unwrap();
    h.csr_write(csr::HSTATUS, hstatus::VTW).unwrap();
    enter(&mut h, &bus, Priv::Supervisor, true, CODE + 0x204);
    h.step(&bus);
    assert_eq!(h.csr.mcause, cause::VIRTUAL_INSN);
}

#[test]
fn pointer_masking_in_vs_mode_and_for_hlv_from_u_mode() {
    // VS-mode, vsatp Bare: henvcfg.PMM = PMLEN 7, zero-extended.
    let tagged = (0x7f << 57) | DATA;
    let (bus, mut h) = hart(&[]);
    h.csr_write(csr::HENVCFG, 2 << envcfg::PMM_SHIFT).unwrap();
    bus.write(CODE + 0x104, AccessSize::Word, u64::from(ld(A0, T0)))
        .unwrap();
    enter(&mut h, &bus, Priv::Supervisor, true, CODE + 0x104);
    h.x[T0 as usize] = tagged;
    h.step(&bus);
    assert_eq!(h.pc, CODE + 0x108);
    assert_eq!(h.x[A0 as usize], VAL);

    // HLV in U-mode (hstatus.HU) as VU-mode: hstatus.HUPMM.
    let (bus, mut h) = hart(&[]);
    h.csr_write(csr::HSTATUS, hstatus::HU | (2 << hstatus::HUPMM_SHIFT))
        .unwrap();
    bus.write(CODE + 0x104, AccessSize::Word, u64::from(hlv_d(A0, T0)))
        .unwrap();
    enter(&mut h, &bus, Priv::User, false, CODE + 0x104);
    h.x[T0 as usize] = tagged;
    h.step(&bus);
    assert_eq!(h.pc, CODE + 0x108);
    assert_eq!(h.x[A0 as usize], VAL);
    // Without HUPMM the tagged address is out of range.
    h.csr_write(csr::HSTATUS, hstatus::HU).unwrap();
    enter(&mut h, &bus, Priv::User, false, CODE + 0x104);
    h.x[T0 as usize] = tagged;
    h.step(&bus);
    assert_eq!(h.csr.mcause, cause::LOAD_ACCESS);
}

#[test]
fn debug_translate_walks_both_stages() {
    let (bus, mut h) = hart(&[]);
    let pte = |pa: u64, flags: u64| ((pa >> 12) << 10) | flags;
    let g_root = EMPTY_G_ROOT;
    let vs_root = 0x8002_0000;
    // G-stage: GPA 2-3 GiB -> PA 2-3 GiB, GPA 4-5 GiB -> PA 2-3 GiB.
    bus.write(g_root + 2 * 8, AccessSize::Double, pte(0x8000_0000, 0xdf))
        .unwrap();
    bus.write(g_root + 4 * 8, AccessSize::Double, pte(0x8000_0000, 0xdf))
        .unwrap();
    // VS-stage (its table at GPA = PA 0x8002_0000): GVA 3-4 GiB -> GPA 4-5 GiB.
    bus.write(
        vs_root + 3 * 8,
        AccessSize::Double,
        pte(0x1_0000_0000, 0xcf),
    )
    .unwrap();
    h.csr_write(csr::HGATP, (8 << 60) | (g_root >> 12)).unwrap();
    h.csr_write(csr::VSATP, (8 << 60) | (vs_root >> 12))
        .unwrap();
    bus.write(CODE + 0x104, AccessSize::Word, 0x13).unwrap(); // nop
    enter(&mut h, &bus, Priv::Supervisor, true, CODE + 0x104);
    assert_eq!(h.debug_translate(&bus, 0xc003_0010), Some(0x8003_0010));
    // Unmapped in the VS-stage, and outside the G-stage.
    assert_eq!(h.debug_translate(&bus, 0x4000_0000), None);
    h.csr_write(csr::VSATP, 0).unwrap();
    assert_eq!(h.debug_translate(&bus, 0x1_0003_0010), Some(0x8003_0010));
    assert_eq!(h.debug_translate(&bus, 0xc000_0000), None);
}

#[test]
fn counter_filters_and_triggers_have_vs_and_vu_bits() {
    use rivemu_cpu_riscv::csr::{hpmevent, trig};
    let (bus, mut h) = hart(&[]);
    // VSINH stops minstret in VS-mode but SINH does not.
    h.csr_write(csr::MINSTRETCFG, hpmevent::SINH).unwrap();
    assert_eq!(h.csr_read(csr::MINSTRETCFG).unwrap(), hpmevent::SINH);
    h.csr_write(csr::MINSTRETCFG, hpmevent::VSINH | hpmevent::VUINH)
        .unwrap();
    assert_eq!(
        h.csr_read(csr::MINSTRETCFG).unwrap(),
        hpmevent::VSINH | hpmevent::VUINH
    );
    for i in 0..3 {
        bus.write(CODE + 0x104 + 4 * i, AccessSize::Word, 0x13)
            .unwrap(); // nop
    }
    enter(&mut h, &bus, Priv::Supervisor, true, CODE + 0x104);
    let before = h.csr.minstret;
    h.step(&bus);
    h.step(&bus);
    assert_eq!(h.csr.minstret, before, "VSINH: no count in VS-mode");
    h.csr.minstretcfg = hpmevent::SINH;
    h.step(&bus);
    assert_eq!(h.csr.minstret, before + 1, "SINH does not apply to VS-mode");

    // An mcontrol6 execute trigger with only `s` set does not fire in
    // VS-mode; with `vs` it does.
    let (bus, mut h) = hart(&[]);
    bus.write(CODE + 0x104, AccessSize::Word, 0x13).unwrap();
    h.csr_write(csr::TSELECT, 0).unwrap();
    let t1 = (trig::TYPE_MCONTROL6 << trig::TYPE_SHIFT) | trig::EXECUTE;
    h.csr_write(csr::TDATA2, CODE + 0x104).unwrap();
    h.csr_write(csr::TDATA1, t1 | trig::S).unwrap();
    enter(&mut h, &bus, Priv::Supervisor, true, CODE + 0x104);
    h.step(&bus);
    assert_eq!(h.pc, CODE + 0x108, "s alone: no match in VS-mode");
    h.csr_write(csr::TDATA1, t1 | trig::VS6).unwrap();
    assert_eq!(h.csr_read(csr::TDATA1).unwrap(), t1 | trig::VS6);
    bus.write(CODE + 0x108, AccessSize::Word, 0x73).unwrap(); // ecall
    h.step(&bus);
    enter(&mut h, &bus, Priv::Supervisor, true, CODE + 0x104);
    h.step(&bus);
    assert_eq!(h.csr.mcause, cause::BREAKPOINT);
    assert_eq!(h.csr.mepc, CODE + 0x104);
}
