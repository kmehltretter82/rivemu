# riscv-arch-test (ACT4) privileged subset

`rivemu-rv64/` is a DUT configuration for the
[riscv-arch-test](https://github.com/riscv-non-isa/riscv-arch-test) framework.
The framework builds self-checking ELFs: it runs each test on the Sail
reference model, records the expected results, and links them into the test.
Every ELF reports its own pass or fail, so running it on rivemu needs no Sail.

## Results

On 2026-11-02, with riscv-arch-test `2dc36dd`, Sail 0.16 (debug-v1 build) and
clang 20.1.2, all 418 tests in the 15 privileged suites passed:

- suites: `InterruptsS`, `InterruptsSm`, `PMP`, `S`, `Sm`, `Sstc`, `Sv`,
  `Svade`, `Svadu`, `Svbare`, `Svinval`, `Svnapot`, `Svpbmt`, `Svvptc`, `U`;
- excluded: `SsstrictV`, which Sail does not model yet.

| rivemu | Passed | Failed |
|---|---|---|
| `29434f1` (before the fixes below) | 411 | 7 |
| with the fixes | 418 | 0 |

Logs: [`docs/evidence/2026-11-02/arch-test/`](../../docs/evidence/2026-11-02/arch-test/).

The 43 unprivileged `rv64i` suites (833 tests: I, M, A/Zaamo/Zalrsc/Zabha/Zacas, F, D,
Zfh/Zfhmin/Zfbfmin/Zfa, C/Zca/Zcb/Zcd/Zcmop, B and the scalar-crypto suites, Zicond, Zimop,
Zicbo*, Zicsr, Zicntr/Zihpm and others) also pass: **833/833**
(`summary-rv64i.txt`). Build them with the same command, using
`EXTENSIONS=$(ls tests/rv64i | paste -sd,)`.

The 7 failures were rivemu bugs:

- `Sm_mcsr_cntr-00`:
  - `mcycle` kept counting with `mcountinhibit.CY = 1`, because it was
    an alias of `minstret`;
  - after that was fixed, `time` did not follow a guest write to
    `mtime`.
- `Sv_Zicbo_sv{39,48,57}_zicbom_exceptions_{S,U}mode-00`:
  `cbo.clean`/`flush`/`inval` to unmapped physical memory did not raise a
  store/AMO access fault.

## Configuration

The configuration is derived from riscv-arch-test's
`config/qemu/qemu-rv64-max`. It describes what rivemu implements, not what
QEMU implements.

| File | Origin | Changes |
|---|---|---|
| `rivemu-rv64.yaml` (UDB) | `qemu-rv64-max.yaml` | Removed: H and the hypervisor-dependent Sh\* extensions, Smaia/Ssaia, Smcdeleg, Smcsrind/Sscsrind, Smctr/Ssctr, Ssccfg, Q, Zv\* crypto/FP16/BF16, B. Added: Svukte, Zvl256b. `VLEN` 256; `MARCHID_IMPLEMENTED` false; delegation masks match rivemu. |
| `sail.json` | `qemu-rv64-max/sail.json` | The same extensions disabled. `medeleg` delegatable bits `0x4b3ff` (rivemu's `MEDELEG_MASK`); `vlen_exp` 8; `archid` 0. Zama16b: RAM has a 16-byte misaligned-atomicity granule, and misaligned AMOs crossing it raise address-misaligned (rivemu's choice; the spec also allows an access fault). |
| `rvmodel_macros.h` | `qemu-rv64-m/rvmodel_macros.h`, unchanged | rivemu's `virt` has the same CLINT, PLIC, UART and semihosting exit. |
| `link.ld` | `qemu-rv64-max/link.ld`, unchanged | |

Two of these follow from implementation choices that conforming designs may
make either way, and rivemu reports them as follows:

- **misa.B:** rivemu reads 0 even though Zba, Zbb and Zbs are implemented.
  QEMU's `rv64` model does the same.
- **Misaligned AMOs outside a 16-byte granule:** rivemu raises
  address-misaligned. Inside a granule they execute (Zama16b).

The first run still had QEMU's VLEN (128), B, archid (5) and misaligned-AMO
settings, with Svukte off. It failed 21 tests: 14 from those settings and 7
from the rivemu bugs above.

## Building the ELFs

`scripts/build-arch-test.sh <work-dir>` does all of the steps below from
pinned inputs: the Sail compiler, the Sail model, riscv-arch-test and the
ELFs. The CI job `arch-test` runs it, caches the ELFs by configuration
hash, and requires every test to pass. To build by hand:

Requirements:

- Sail 0.16 (`rivemu-lockstep/scripts/build-sail-debug-v1.sh`);
- clang/LLVM 20;
- Ruby with the `udb` gem;
- `uv`.

See the riscv-arch-test README for details.

```bash
cd riscv-arch-test
export PATH=/path/to/sail-riscv-0.16/bin:$PATH
make elfs CONFIG_FILES=/path/to/rivemu/tests/arch-test/rivemu-rv64/test_config.yaml \
  WORKDIR=$PWD/work \
  EXTENSIONS=InterruptsS,InterruptsSm,PMP,S,Sm,Sstc,Sv,Svade,Svadu,Svbare,Svinval,Svnapot,Svpbmt,Svvptc,U \
  EXCLUDE_EXTENSIONS=SsstrictV ENABLE_EXPERIMENTAL_EXTENSIONS=True JOBS=4
```

`ENABLE_EXPERIMENTAL_EXTENSIONS=True` passes `--enable-experimental-extensions`
to Sail. Sail 0.16 models Svukte only with that flag
(`hartSupports(Ext_Svukte)`). Without it, the `senvcfg.UKTE` checks in
`S_scsr` and `Sm_scsr_from_m` expect the bit to be read-only zero.

## Running on rivemu

```bash
cargo build --release -p rivemu-cli
scripts/run-arch-test.sh "$PWD/work/rivemu-rv64/elfs" /tmp/act-results
```

A test passes when rivemu exits 0 and the test printed `RVCP-SUMMARY: TEST
PASSED`. On failure, the test prints the failing coverpoint, the instruction,
and the expected and actual values. For a trap test it prints the expected and
actual `xcause`, `xepc` and `xtval` instead.
