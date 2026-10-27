/*
 * SPDX-FileCopyrightText: 2026 roolrz
 * SPDX-License-Identifier: Apache-2.0
 */

#include "asm_constants.h"

.section .text.native, "ax"
.balign 4
.global riscv64_run_native_user
.type riscv64_run_native_user, @function
riscv64_run_native_user:
    # The caller owns a masked CPU and active Sv39 root. The anchor is private
    # kernel storage; only the final U return publishes it through sscratch.
    addi sp, sp, -USER_ANCHOR_SIZE
    sd ra, 0(sp)
    sd s0, 8(sp)
    sd s1, 16(sp)
    sd s2, 24(sp)
    sd s3, 32(sp)
    sd s4, 40(sp)
    sd s5, 48(sp)
    sd s6, 56(sp)
    sd s7, 64(sp)
    sd s8, 72(sp)
    sd s9, 80(sp)
    sd s10, 88(sp)
    sd s11, 96(sp)
    sd gp, USER_ANCHOR_GP_OFFSET(sp)
    sd tp, USER_ANCHOR_TP_OFFSET(sp)
    sd a0, USER_ANCHOR_CONTEXT_OFFSET(sp)
    csrr t0, sstatus
    sd t0, USER_ANCHOR_STATUS_OFFSET(sp)
    csrr t0, stvec
    sd t0, USER_ANCHOR_VECTOR_OFFSET(sp)
    csrr t0, hstatus
    sd t0, USER_ANCHOR_HSTATUS_OFFSET(sp)
    csrr t0, scounteren
    sd t0, USER_ANCHOR_COUNTER_OFFSET(sp)
    csrr t0, senvcfg
    sd t0, USER_ANCHOR_ENVCFG_OFFSET(sp)
    csrw scounteren, zero
    csrw senvcfg, zero
    # HU must be clear: Native applications have no authority to issue HLV/HSV.
    li t0, HSTATUS_NATIVE_CLEAR
    csrc hstatus, t0
    # Initialize the reusable trap frame from the pinned stopped context.
    mv t0, a0
    addi sp, sp, -USER_FRAME_SIZE
    mv t1, sp
    li t2, USER_FRAME_SIZE
1:
    ld t3, 0(t0)
    sd t3, 0(t1)
    addi t0, t0, 8
    addi t1, t1, 8
    addi t2, t2, -8
    bnez t2, 1b
    # Data coherence does not imply instruction-fetch coherence.
    fence.i
    j .Lnative_restore
.size riscv64_run_native_user, . - riscv64_run_native_user

.balign 4
.global riscv64_native_vector
.type riscv64_native_vector, @function
riscv64_native_vector:
    csrrw sp, sscratch, sp
    addi sp, sp, -USER_FRAME_SIZE
    sd zero, 0(sp)
    sd ra, 8(sp)
    sd gp, 24(sp)
    sd tp, 32(sp)
    sd t0, 40(sp)
    sd t1, 48(sp)
    sd t2, 56(sp)
    sd s0, 64(sp)
    sd s1, 72(sp)
    sd a0, 80(sp)
    sd a1, 88(sp)
    sd a2, 96(sp)
    sd a3, 104(sp)
    sd a4, 112(sp)
    sd a5, 120(sp)
    sd a6, 128(sp)
    sd a7, 136(sp)
    sd s2, 144(sp)
    sd s3, 152(sp)
    sd s4, 160(sp)
    sd s5, 168(sp)
    sd s6, 176(sp)
    sd s7, 184(sp)
    sd s8, 192(sp)
    sd s9, 200(sp)
    sd s10, 208(sp)
    sd s11, 216(sp)
    sd t3, 224(sp)
    sd t4, 232(sp)
    sd t5, 240(sp)
    sd t6, 248(sp)
    csrr t0, sscratch
    sd t0, 16(sp)
    addi t0, sp, USER_FRAME_SIZE
    ld gp, USER_ANCHOR_GP_OFFSET(t0)
    ld tp, USER_ANCHOR_TP_OFFSET(t0)
    csrw sscratch, zero
    # Reestablish the host vector before any Rust call or nested exception.
    la t0, riscv64_trap_vector
    csrw stvec, t0
    csrr t0, sepc
    sd t0, USER_FRAME_PC_OFFSET(sp)
    csrr t0, sstatus
    sd t0, USER_FRAME_STATUS_OFFSET(sp)
    csrr t0, scause
    sd t0, USER_FRAME_CAUSE_OFFSET(sp)
    csrr t0, stval
    sd t0, USER_FRAME_FAULT_OFFSET(sp)
    li t0, SSTATUS_FS_MASK
    csrc sstatus, t0
    addi t0, sp, USER_FRAME_SIZE
    ld t1, USER_ANCHOR_COUNTER_OFFSET(t0)
    csrw scounteren, t1
    ld t1, USER_ANCHOR_ENVCFG_OFFSET(t0)
    csrw senvcfg, t1
    ld t1, USER_ANCHOR_HSTATUS_OFFSET(t0)
    csrw hstatus, t1
    ld t1, USER_ANCHOR_STATUS_OFFSET(t0)
    csrw sstatus, t1
    mv a0, sp
    call riscv64_native_trap_dispatch
    bnez a0, riscv64_unwind_native_user
.Lnative_restore:
    # Only masked assembly installs a Native return regime. Rust always sees
    # the host vector, zero sscratch, and privileged access protection.
    ld t0, USER_FRAME_PC_OFFSET(sp)
    csrw sepc, t0
    ld t0, USER_FRAME_STATUS_OFFSET(sp)
    csrw sstatus, t0
    li t0, HSTATUS_NATIVE_CLEAR
    csrc hstatus, t0
    csrw scounteren, zero
    csrw senvcfg, zero
    la t0, riscv64_native_vector
    csrw stvec, t0
    addi t0, sp, USER_FRAME_SIZE
    csrw sscratch, t0
    ld ra, 8(sp)
    ld gp, 24(sp)
    ld tp, 32(sp)
    ld t0, 40(sp)
    ld t1, 48(sp)
    ld t2, 56(sp)
    ld s0, 64(sp)
    ld s1, 72(sp)
    ld a0, 80(sp)
    ld a1, 88(sp)
    ld a2, 96(sp)
    ld a3, 104(sp)
    ld a4, 112(sp)
    ld a5, 120(sp)
    ld a6, 128(sp)
    ld a7, 136(sp)
    ld s2, 144(sp)
    ld s3, 152(sp)
    ld s4, 160(sp)
    ld s5, 168(sp)
    ld s6, 176(sp)
    ld s7, 184(sp)
    ld s8, 192(sp)
    ld s9, 200(sp)
    ld s10, 208(sp)
    ld s11, 216(sp)
    ld t3, 224(sp)
    ld t4, 232(sp)
    ld t5, 240(sp)
    ld t6, 248(sp)
    ld sp, 16(sp)
    sret
.size riscv64_native_vector, . - riscv64_native_vector

.global riscv64_refresh_native_anchor
.type riscv64_refresh_native_anchor, @function
riscv64_refresh_native_anchor:
    # A blocked syscall's private anchor follows its Thread. Kernel TP names
    # the current hart, while the user's TLS pointer stays in frame register x4.
    addi a0, a0, USER_FRAME_SIZE
    sd tp, USER_ANCHOR_TP_OFFSET(a0)
    ret
.size riscv64_refresh_native_anchor, . - riscv64_refresh_native_anchor

.global riscv64_unwind_native_user
.type riscv64_unwind_native_user, @function
riscv64_unwind_native_user:
    # The dispatcher copied all owned state and closed publication. No raw
    # frame or borrowed service crosses the restored host continuation.
    addi sp, sp, USER_FRAME_SIZE
    csrw sscratch, zero
    ld t0, USER_ANCHOR_VECTOR_OFFSET(sp)
    csrw stvec, t0
    ld ra, 0(sp)
    ld s0, 8(sp)
    ld s1, 16(sp)
    ld s2, 24(sp)
    ld s3, 32(sp)
    ld s4, 40(sp)
    ld s5, 48(sp)
    ld s6, 56(sp)
    ld s7, 64(sp)
    ld s8, 72(sp)
    ld s9, 80(sp)
    ld s10, 88(sp)
    ld s11, 96(sp)
    ld gp, USER_ANCHOR_GP_OFFSET(sp)
    ld tp, USER_ANCHOR_TP_OFFSET(sp)
    addi sp, sp, USER_ANCHOR_SIZE
    ret
.size riscv64_unwind_native_user, . - riscv64_unwind_native_user

.global riscv64_call_native_irq_on_stack
.type riscv64_call_native_irq_on_stack, @function
riscv64_call_native_irq_on_stack:
    # Native dispatch returns one scalar; the host/guest trap-stack helper has
    # a separate two-register action ABI and cannot share its Rust declaration.
    addi a1, a1, -16
    sd sp, 0(a1)
    sd ra, 8(a1)
    mv sp, a1
    jalr a2
    ld t0, 0(sp)
    ld ra, 8(sp)
    mv sp, t0
    ret
.size riscv64_call_native_irq_on_stack, . - riscv64_call_native_irq_on_stack
