/*
 * Linker script for the riscv-tests "v" environment when linking with LLD.
 *
 * Same layout as upstream env/v/link.ld, plus explicit placement of the
 * sections that upstream leaves as orphans. GNU ld puts orphan read-only data
 * after .text, but LLD puts .rodata.* at the start of the image, ahead of
 * .text.init. That breaks loaders that jump to 0x80000000 instead of the ELF
 * entry point (Spike, QEMU `-M spike`).
 */
OUTPUT_ARCH( "riscv" )
ENTRY(_start)

SECTIONS
{
  . = 0x80000000;
  .text.init : { *(.text.init) }
  . = ALIGN(0x1000);
  .tohost : { *(.tohost) }
  . = ALIGN(0x1000);
  .text : { *(.text) *(.text.*) }
  .rodata : { *(.rodata) *(.rodata.*) *(.srodata) *(.srodata.*) }
  . = ALIGN(0x1000);
  .data : { *(.data) }
  .bss : { *(.bss) }
  _end = .;
}
