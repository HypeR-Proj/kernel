/*
 * SPDX-FileCopyrightText: 2026 roolrz
 * SPDX-License-Identifier: Apache-2.0
 */

#include "asm_constants.h"

.section .text.context, "ax"

.global riscv64_switch_context
.type riscv64_switch_context, @function
riscv64_switch_context:
    sd s0, 0(a0)
    sd s1, 8(a0)
    sd s2, 16(a0)
    sd s3, 24(a0)
    sd s4, 32(a0)
    sd s5, 40(a0)
    sd s6, 48(a0)
    sd s7, 56(a0)
    sd s8, 64(a0)
    sd s9, 72(a0)
    sd s10, 80(a0)
    sd s11, 88(a0)
    sd ra, THREAD_CONTEXT_RA_OFFSET(a0)
    sd sp, THREAD_CONTEXT_SP_OFFSET(a0)
    // Retain only SSTATUS.SIE from the guard's complete saved SSTATUS value.
    andi t0, a2, 2
    sd t0, THREAD_CONTEXT_SIE_OFFSET(a0)

    // Completion runs on the incoming stack while SIE remains clear. s0 was
    // saved above and is preserved by the callback until next state is loaded.
    mv s0, a1
    ld sp, THREAD_CONTEXT_SP_OFFSET(s0)
    mv a0, a4
    jalr a3
    mv a1, s0

    ld s0, 0(a1)
    ld s1, 8(a1)
    ld s2, 16(a1)
    ld s3, 24(a1)
    ld s4, 32(a1)
    ld s5, 40(a1)
    ld s6, 48(a1)
    ld s7, 56(a1)
    ld s8, 64(a1)
    ld s9, 72(a1)
    ld s10, 80(a1)
    ld s11, 88(a1)
    ld ra, THREAD_CONTEXT_RA_OFFSET(a1)
    ld t0, THREAD_CONTEXT_SIE_OFFSET(a1)
    beqz t0, .Lswitch_irq_disabled
    csrsi sstatus, 2
    ret
.Lswitch_irq_disabled:
    csrci sstatus, 2
    ret
.size riscv64_switch_context, . - riscv64_switch_context

.global riscv64_thread_trampoline
.type riscv64_thread_trampoline, @function
riscv64_thread_trampoline:
    mv a0, s1
    jalr s0
    call kernel_thread_exit
1:  wfi
    j 1b
.size riscv64_thread_trampoline, . - riscv64_thread_trampoline

.global riscv64_reset_stack_and_enter
.type riscv64_reset_stack_and_enter, @function
riscv64_reset_stack_and_enter:
    mv sp, a1
    mv s0, zero
    mv ra, zero
    sd a3, 0(a0)
    addi a0, a0, 8
1:  bgeu a0, a1, 2f
    sd a2, 0(a0)
    addi a0, a0, 8
    j 1b
2:  mv t0, a4
    mv a0, a5
    jr t0
.size riscv64_reset_stack_and_enter, . - riscv64_reset_stack_and_enter

.global riscv64_run_on_stack
.type riscv64_run_on_stack, @function
riscv64_run_on_stack:
    mv sp, a0
    mv s0, zero
    mv ra, zero
    mv t0, a1
    mv a0, a2
    jr t0
.size riscv64_run_on_stack, . - riscv64_run_on_stack
