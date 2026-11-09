/* M-mode entry: identity-map the first 4 GiB with Sv39 gigapages, run
 * bench_main in S-mode, print the result, power off (SiFive test device). */
    .option nopic
    .text
    .globl _start
_start:
    la sp, stack_top
    li t0, -1
    csrw pmpaddr0, t0
    li t0, 0x1f
    csrw pmpcfg0, t0
    /* Sv39 root: entries 0..3 map 0..4 GiB, RWXAD, global. */
    la t0, root
    li t1, 0
    li t2, 4
1:  slli t3, t1, 28                 /* (i << 30) >> 12 << 10 */
    ori t3, t3, 0xef                 /* V R W X G A D */
    sd t3, 0(t0)
    addi t0, t0, 8
    addi t1, t1, 1
    blt t1, t2, 1b
    la t0, root
    srli t0, t0, 12
    li t1, 8 << 60
    or t0, t0, t1
    csrw satp, t0
    sfence.vma
    li t0, 0x1800
    csrc mstatus, t0
    li t0, 0x800                    /* MPP = S */
    csrs mstatus, t0
    la t0, s_entry
    csrw mepc, t0
    la t0, m_trap
    csrw mtvec, t0
    mret
s_entry:
    call bench_main
    ecall                           /* back to M-mode */
    .balign 4                       /* mtvec needs 4-byte alignment */
m_trap:
    li t0, 0x100000
    li t1, 0x5555
    sw t1, 0(t0)
2:  j 2b

    .bss
    .balign 4096
root:
    .space 4096
    .space 65536
stack_top:
