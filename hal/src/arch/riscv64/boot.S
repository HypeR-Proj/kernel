/*
 * SPDX-FileCopyrightText: 2026 roolrz
 * SPDX-License-Identifier: Apache-2.0
 */

#include "asm_constants.h"

.option push
.option norvc
.section .text.boot, "ax"
.global _start
.type _start, @function
_start:
    j .Lboot_entry
    nop
    .dword 0x00200000
    .dword __image_size
    .dword 0
    .word 2
    .word 0
    .dword 0
    .ascii "RISCV\0\0\0"
    .byte 0x52, 0x53, 0x43, 0x05
    .word 0
.Lboot_entry:
    rdtime s11
    csrw sie, zero
    csrci sstatus, 2
    mv s8, a0
    mv s9, a1
    lla s10, _start
    csrw sscratch, zero

    call riscv64_apply_load_relocations
    call riscv64_clear_bss

    lla sp, __boot_stack_top
    li t0, -16
    and sp, sp, t0
    mv s0, zero
    mv ra, zero
    mv a0, s8
    mv a1, s9
    mv a2, s11
    call hyper_bootstrap
.Lboot_halt:
    wfi
    j .Lboot_halt
.size _start, . - _start

.type riscv64_apply_load_relocations, @function
riscv64_apply_load_relocations:
    lla t0, __rela_dyn_start
    lla t1, __rela_dyn_end
.Lrela_load_loop:
    bgeu t0, t1, .Lrela_load_done
    ld t2, 0(t0)
    ld t3, 8(t0)
    ld t4, 16(t0)
    li t5, R_RISCV_RELATIVE
    bne t3, t5, .Lboot_halt
    add t2, s10, t2
    add t4, s10, t4
    sd t4, 0(t2)
    addi t0, t0, 24
    j .Lrela_load_loop
.Lrela_load_done:
    ret
.size riscv64_apply_load_relocations, . - riscv64_apply_load_relocations

.type riscv64_clear_bss, @function
riscv64_clear_bss:
    lla t0, __bss_start
    lla t1, __bss_end
.Lbss_loop:
    bgeu t0, t1, .Lbss_done
    sd zero, 0(t0)
    addi t0, t0, 8
    j .Lbss_loop
.Lbss_done:
    ret
.size riscv64_clear_bss, . - riscv64_clear_bss

/*
 * a0 = root page-table physical address
 * a1 = high kernel virtual base
 * a2 = high bootstrap stack top
 */
.global riscv64_activate_final_address_space
.type riscv64_activate_final_address_space, @function
riscv64_activate_final_address_space:
    csrci sstatus, 2
    mv s0, a0
    mv s1, a1
    mv s2, a2
    lla s3, _start

    /* Rewrite PIE pointers for the permanent high mapping. */
    lla t0, __rela_dyn_start
    lla t1, __rela_dyn_end
.Lrela_virtual_loop:
    bgeu t0, t1, .Lrela_virtual_done
    ld t2, 0(t0)
    ld t3, 8(t0)
    ld t4, 16(t0)
    li t5, R_RISCV_RELATIVE
    bne t3, t5, .Lboot_halt
    add t2, s3, t2
    add t4, s1, t4
    sd t4, 0(t2)
    addi t0, t0, 24
    j .Lrela_virtual_loop
.Lrela_virtual_done:
    srli t0, s0, PAGE_SHIFT
    li t1, SATP_MODE_SV39
    or t0, t0, t1
    sfence.vma
    csrw satp, t0
    sfence.vma

    lla t0, start_kernel
    sub t0, t0, s3
    add t0, t0, s1
    mv sp, s2
    mv s0, zero
    mv ra, zero
    jr t0
.size riscv64_activate_final_address_space, . - riscv64_activate_final_address_space

/* OpenSBI HSM entry: a0 = hart ID, a1 = parameter-record physical address. */
.global riscv64_secondary_entry
.type riscv64_secondary_entry, @function
riscv64_secondary_entry:
    csrci sstatus, 2
    csrw sie, zero
    mv t6, a1
    ld t0, 0(t6)
    ld sp, 8(t6)
    ld t1, 24(t6)
    mv tp, t1
    csrw sscratch, zero
    srli t0, t0, PAGE_SHIFT
    li t1, SATP_MODE_SV39
    or t0, t0, t1
    sfence.vma
    csrw satp, t0
    sfence.vma
    ld sp, 16(t6)
    ld t0, 32(t6)
    mv a0, tp
    mv s0, zero
    mv ra, zero
    jr t0
.size riscv64_secondary_entry, . - riscv64_secondary_entry
.option pop
