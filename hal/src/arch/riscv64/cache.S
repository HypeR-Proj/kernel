/*
 * SPDX-FileCopyrightText: 2026 roolrz
 * SPDX-License-Identifier: Apache-2.0
 */

    .section .text.cache, "ax"
    .balign 4

    .global riscv64_cbo_clean
    .type riscv64_cbo_clean, @function
riscv64_cbo_clean:
    cbo.clean 0(a0)
    ret
    .size riscv64_cbo_clean, . - riscv64_cbo_clean

    .global riscv64_cbo_flush
    .type riscv64_cbo_flush, @function
riscv64_cbo_flush:
    cbo.flush 0(a0)
    ret
    .size riscv64_cbo_flush, . - riscv64_cbo_flush

    .global riscv64_cbo_inval
    .type riscv64_cbo_inval, @function
riscv64_cbo_inval:
    cbo.inval 0(a0)
    ret
    .size riscv64_cbo_inval, . - riscv64_cbo_inval
