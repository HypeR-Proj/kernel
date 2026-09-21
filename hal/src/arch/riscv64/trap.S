/*
 * SPDX-FileCopyrightText: 2026 roolrz
 * SPDX-License-Identifier: Apache-2.0
 */

#include "asm_constants.h"

.section .text.trap, "ax"
.balign 4
.global riscv64_trap_vector
.type riscv64_trap_vector, @function
riscv64_trap_vector:
    # Guest entry leaves the reserved HS stack in sscratch. Host execution
    # keeps sscratch zero, so the same non-clobbering exchange distinguishes
    # both origins before any interrupted GPR is touched.
    csrrw sp, sscratch, sp
    bnez sp, .Lguest_entry
    csrrw sp, sscratch, sp
    addi sp, sp, -TRAP_FRAME_SIZE
    sd zero, TRAP_FRAME_GENERAL_OFFSET(sp)
    sd zero, TRAP_FRAME_GUEST_ORIGIN_OFFSET(sp)
    sd zero, TRAP_FRAME_GUEST_ANCHOR_RETURN_OFFSET(sp)
    sd zero, TRAP_FRAME_GUEST_CONTEXT_OFFSET(sp)
    sd ra, TRAP_FRAME_RA_OFFSET(sp)
    sd t0, TRAP_FRAME_T0_OFFSET(sp)
    addi t0, sp, TRAP_FRAME_SIZE
    sd t0, TRAP_FRAME_SP_OFFSET(sp)
    sd gp, TRAP_FRAME_GP_OFFSET(sp)
    sd tp, TRAP_FRAME_TP_OFFSET(sp)
    sd tp, TRAP_FRAME_HOST_CPU_INDEX_OFFSET(sp)
    li t0, 0
    j .Lsave_remaining
.Lguest_entry:
    addi sp, sp, -TRAP_FRAME_SIZE
    sd zero, TRAP_FRAME_GENERAL_OFFSET(sp)
    sd zero, TRAP_FRAME_GUEST_ORIGIN_OFFSET(sp)
    sd ra, TRAP_FRAME_RA_OFFSET(sp)
    sd t0, TRAP_FRAME_T0_OFFSET(sp)
    csrr t0, sscratch
    sd t0, TRAP_FRAME_SP_OFFSET(sp)
    csrw sscratch, zero
    li t0, 1
.Lsave_remaining:
    sd gp, TRAP_FRAME_GP_OFFSET(sp)
    sd tp, TRAP_FRAME_TP_OFFSET(sp)
    sd t1, TRAP_FRAME_T1_OFFSET(sp)
    sd t2, TRAP_FRAME_T2_OFFSET(sp)
    sd s0, TRAP_FRAME_S0_OFFSET(sp)
    sd s1, TRAP_FRAME_S1_OFFSET(sp)
    sd a0, TRAP_FRAME_A0_OFFSET(sp)
    sd a1, TRAP_FRAME_A1_OFFSET(sp)
    sd a2, TRAP_FRAME_A2_OFFSET(sp)
    sd a3, TRAP_FRAME_A3_OFFSET(sp)
    sd a4, TRAP_FRAME_A4_OFFSET(sp)
    sd a5, TRAP_FRAME_A5_OFFSET(sp)
    sd a6, TRAP_FRAME_A6_OFFSET(sp)
    sd a7, TRAP_FRAME_A7_OFFSET(sp)
    sd s2, TRAP_FRAME_S2_OFFSET(sp)
    sd s3, TRAP_FRAME_S3_OFFSET(sp)
    sd s4, TRAP_FRAME_S4_OFFSET(sp)
    sd s5, TRAP_FRAME_S5_OFFSET(sp)
    sd s6, TRAP_FRAME_S6_OFFSET(sp)
    sd s7, TRAP_FRAME_S7_OFFSET(sp)
    sd s8, TRAP_FRAME_S8_OFFSET(sp)
    sd s9, TRAP_FRAME_S9_OFFSET(sp)
    sd s10, TRAP_FRAME_S10_OFFSET(sp)
    sd s11, TRAP_FRAME_S11_OFFSET(sp)
    sd t3, TRAP_FRAME_T3_OFFSET(sp)
    sd t4, TRAP_FRAME_T4_OFFSET(sp)
    sd t5, TRAP_FRAME_T5_OFFSET(sp)
    sd t6, TRAP_FRAME_T6_OFFSET(sp)
    beqz t0, .Lhost_origin
    li t1, 1
    sd t1, TRAP_FRAME_GUEST_ANCHOR_RETURN_OFFSET(sp)
    addi t0, sp, TRAP_FRAME_SIZE
    ld a0, GUEST_HS_ANCHOR_CONTEXT_OFFSET(t0)
    sd a0, TRAP_FRAME_GUEST_CONTEXT_OFFSET(sp)
    csrr t1, vsstatus
    sd t1, VCPU_VSSTATUS_OFFSET(a0)
    csrr t1, vsie
    sd t1, VCPU_VSIE_OFFSET(a0)
    csrr t1, vstvec
    sd t1, VCPU_VSTVEC_OFFSET(a0)
    csrr t1, vsscratch
    sd t1, VCPU_VSSCRATCH_OFFSET(a0)
    csrr t1, vsepc
    sd t1, VCPU_VSEPC_OFFSET(a0)
    csrr t1, vscause
    sd t1, VCPU_VSCAUSE_OFFSET(a0)
    csrr t1, vstval
    sd t1, VCPU_VSTVAL_OFFSET(a0)
    csrw vsie, zero
    csrr t1, hstatus
    srli t1, t1, 8
    andi t1, t1, 1
    sd t1, VCPU_SUPERVISOR_OFFSET(a0)
    # SCOUNTEREN and SENVCFG have no VS aliases. Capture the guest values
    # before restoring the host copies from the anchor; Rust must never infer
    # them from the host-visible CSRs after this point.
    csrr t1, scounteren
    sd t1, VCPU_SCOUNTEREN_OFFSET(a0)
    csrr t1, senvcfg
    sd t1, VCPU_SENVCFG_OFFSET(a0)
    csrr t1, vsatp
    sd t1, VCPU_VSATP_OFFSET(a0)
    csrw vsatp, zero
    csrr t1, hvip
    li t2, HVIP_GUEST_MASK
    and t1, t1, t2
    sd t1, VCPU_HVIP_OFFSET(a0)
    csrc hvip, t2
    csrr t1, 0x24d
    sd t1, VCPU_VSTIMECMP_OFFSET(a0)
    li t1, -1
    csrw 0x24d, t1
    # No Rust code may run while guest floating state is live. Save it through
    # the exact anchor context, then restore the complete host floating image.
    call riscv64_save_guest_floating_point
    addi a0, sp, TRAP_FRAME_SIZE
    call riscv64_restore_host_floating_point
    addi t0, sp, TRAP_FRAME_SIZE
    ld t1, GUEST_HS_ANCHOR_FS_OFFSET(t0)
    li t2, (3 << 13)
    csrc sstatus, t2
    csrs sstatus, t1
    ld t1, GUEST_HS_ANCHOR_SCOUNTEREN_OFFSET(t0)
    csrw scounteren, t1
    ld t1, GUEST_HS_ANCHOR_SENVCFG_OFFSET(t0)
    csrw senvcfg, t1
    ld gp, GUEST_HS_ANCHOR_GP_OFFSET(t0)
    ld tp, GUEST_HS_ANCHOR_TP_OFFSET(t0)
    sd tp, TRAP_FRAME_HOST_CPU_INDEX_OFFSET(sp)
    csrr t0, hstatus
    li t1, HSTATUS_SPV
    and t1, t0, t1
    sd t1, TRAP_FRAME_GUEST_ORIGIN_OFFSET(sp)
    j .Ldispatch
.Lhost_origin:
    # A zero-sscratch entry is host-origin regardless of stale HSTATUS state,
    # and returns without the guest-anchor swap.
    sd zero, TRAP_FRAME_GUEST_ORIGIN_OFFSET(sp)
.Ldispatch:
    csrr t0, sepc
    sd t0, TRAP_FRAME_SEPC_OFFSET(sp)
    csrr t0, sstatus
    sd t0, TRAP_FRAME_SSTATUS_OFFSET(sp)
    csrr t0, scause
    sd t0, TRAP_FRAME_SCAUSE_OFFSET(sp)
    csrr t0, stval
    sd t0, TRAP_FRAME_STVAL_OFFSET(sp)
    csrr t0, htval
    sd t0, TRAP_FRAME_HTVAL_OFFSET(sp)
    csrr t0, hstatus
    sd t0, TRAP_FRAME_HSTATUS_OFFSET(sp)
    # Host nested traps must not inherit a guest return domain.
    li t0, ((1 << 7) | (1 << 9))
    csrc hstatus, t0
    csrr t0, htinst
    sd t0, TRAP_FRAME_HTINST_OFFSET(sp)
    mv a0, sp
    call riscv64_trap_dispatch
    beqz a0, .Lresume_trap
    li t0, TRAP_ACTION_POSTLUDE
    beq a0, t0, .Lrun_postlude
    li t0, TRAP_ACTION_ANCHOR_IRQ_TAIL
    beq a0, t0, .Lanchor_irq_tail
    li t0, TRAP_ACTION_ANCHOR_STOPPED
    beq a0, t0, .Lanchor_stopped
.Linvalid_trap_action:
    mv t0, a0
    mv t1, a1
    mv a0, sp
    mv a1, t0
    mv a2, t1
    csrw sscratch, zero
    call riscv64_invalid_trap_action
.Lrun_postlude:
    beqz a1, .Linvalid_trap_action
    jalr a1
    j .Lresume_trap
.Lanchor_irq_tail:
    beqz a1, .Linvalid_trap_action
    # The dispatcher captured all guest state and published IRQ_TAIL last.
    # Destroy the sscratch anchor before any host continuation can trap, then
    # restore the complete host callee ABI and return the opaque callback.
    li t5, GUEST_ANCHOR_EXIT_IRQ_TAIL
    j .Lanchor_unwind
.Lanchor_stopped:
    li t5, GUEST_ANCHOR_EXIT_STOPPED
.Lanchor_unwind:
    mv t6, a1
    csrw sscratch, zero
    addi sp, sp, TRAP_FRAME_SIZE
    ld ra, GUEST_HS_ANCHOR_RA_OFFSET(sp)
    ld s0, GUEST_HS_ANCHOR_S0_OFFSET + 0(sp)
    ld s1, GUEST_HS_ANCHOR_S0_OFFSET + 8(sp)
    ld s2, GUEST_HS_ANCHOR_S0_OFFSET + 16(sp)
    ld s3, GUEST_HS_ANCHOR_S0_OFFSET + 24(sp)
    ld s4, GUEST_HS_ANCHOR_S0_OFFSET + 32(sp)
    ld s5, GUEST_HS_ANCHOR_S0_OFFSET + 40(sp)
    ld s6, GUEST_HS_ANCHOR_S0_OFFSET + 48(sp)
    ld s7, GUEST_HS_ANCHOR_S0_OFFSET + 56(sp)
    ld s8, GUEST_HS_ANCHOR_S0_OFFSET + 64(sp)
    ld s9, GUEST_HS_ANCHOR_S0_OFFSET + 72(sp)
    ld s10, GUEST_HS_ANCHOR_S0_OFFSET + 80(sp)
    ld s11, GUEST_HS_ANCHOR_S0_OFFSET + 88(sp)
    ld gp, GUEST_HS_ANCHOR_GP_OFFSET(sp)
    ld tp, GUEST_HS_ANCHOR_TP_OFFSET(sp)
    addi sp, sp, GUEST_HS_ANCHOR_SIZE
    mv a0, t5
    mv a1, t6
    ret
.Lresume_trap:
    # Guest trap entry saved its floating image before Rust policy. Reload it
    # after the optional policy callback and before restoring guest GPRs.
    ld t0, TRAP_FRAME_GUEST_ANCHOR_RETURN_OFFSET(sp)
    beqz t0, .Lrestore_trap
    addi t0, sp, TRAP_FRAME_SIZE
    ld a0, GUEST_HS_ANCHOR_CONTEXT_OFFSET(t0)
    call riscv64_load_guest_floating_point
    addi t0, sp, TRAP_FRAME_SIZE
    ld t0, GUEST_HS_ANCHOR_CONTEXT_OFFSET(t0)
    ld t1, VCPU_VSSTATUS_OFFSET(t0)
    csrw vsstatus, t1
    ld t1, VCPU_VSIE_OFFSET(t0)
    csrw vsie, t1
    ld t1, VCPU_VSTVEC_OFFSET(t0)
    csrw vstvec, t1
    ld t1, VCPU_VSSCRATCH_OFFSET(t0)
    csrw vsscratch, t1
    ld t1, VCPU_VSEPC_OFFSET(t0)
    csrw vsepc, t1
    ld t1, VCPU_VSCAUSE_OFFSET(t0)
    csrw vscause, t1
    ld t1, VCPU_VSTVAL_OFFSET(t0)
    csrw vstval, t1
    ld t1, VCPU_SCOUNTEREN_OFFSET(t0)
    csrw scounteren, t1
    ld t1, VCPU_SENVCFG_OFFSET(t0)
    csrw senvcfg, t1
    ld t1, VCPU_VSATP_OFFSET(t0)
    csrw vsatp, t1
    hfence.vvma zero, zero
    li t1, HVIP_GUEST_MASK
    csrc hvip, t1
    ld t2, VCPU_HVIP_OFFSET(t0)
    and t2, t2, t1
    csrs hvip, t2
    li t1, (1 << 63)
    csrs henvcfg, t1
    ld t1, VCPU_VSTIMECMP_OFFSET(t0)
    csrw 0x24d, t1
.Lrestore_trap:
    ld t0, TRAP_FRAME_HSTATUS_OFFSET(sp)
    csrw hstatus, t0
    ld t0, TRAP_FRAME_SEPC_OFFSET(sp)
    csrw sepc, t0
    ld t0, TRAP_FRAME_SSTATUS_OFFSET(sp)
    csrw sstatus, t0
    ld ra, TRAP_FRAME_RA_OFFSET(sp)
    ld gp, TRAP_FRAME_GP_OFFSET(sp)
    ld tp, TRAP_FRAME_TP_OFFSET(sp)
    ld t1, TRAP_FRAME_T1_OFFSET(sp)
    ld t2, TRAP_FRAME_T2_OFFSET(sp)
    ld s0, TRAP_FRAME_S0_OFFSET(sp)
    ld s1, TRAP_FRAME_S1_OFFSET(sp)
    ld a0, TRAP_FRAME_A0_OFFSET(sp)
    ld a1, TRAP_FRAME_A1_OFFSET(sp)
    ld a2, TRAP_FRAME_A2_OFFSET(sp)
    ld a3, TRAP_FRAME_A3_OFFSET(sp)
    ld a4, TRAP_FRAME_A4_OFFSET(sp)
    ld a5, TRAP_FRAME_A5_OFFSET(sp)
    ld a6, TRAP_FRAME_A6_OFFSET(sp)
    ld a7, TRAP_FRAME_A7_OFFSET(sp)
    ld s2, TRAP_FRAME_S2_OFFSET(sp)
    ld s3, TRAP_FRAME_S3_OFFSET(sp)
    ld s4, TRAP_FRAME_S4_OFFSET(sp)
    ld s5, TRAP_FRAME_S5_OFFSET(sp)
    ld s6, TRAP_FRAME_S6_OFFSET(sp)
    ld s7, TRAP_FRAME_S7_OFFSET(sp)
    ld s8, TRAP_FRAME_S8_OFFSET(sp)
    ld s9, TRAP_FRAME_S9_OFFSET(sp)
    ld s10, TRAP_FRAME_S10_OFFSET(sp)
    ld s11, TRAP_FRAME_S11_OFFSET(sp)
    ld t3, TRAP_FRAME_T3_OFFSET(sp)
    ld t4, TRAP_FRAME_T4_OFFSET(sp)
    ld t5, TRAP_FRAME_T5_OFFSET(sp)
    ld t6, TRAP_FRAME_T6_OFFSET(sp)
    # T0 remains scratch until the selected epilogue reloads its interrupted
    # value. Every other GPR, including T1, is already live guest/host state.
    ld t0, TRAP_FRAME_GUEST_ANCHOR_RETURN_OFFSET(sp)
    bnez t0, .Lguest_return
    ld t0, TRAP_FRAME_T0_OFFSET(sp)
    ld sp, TRAP_FRAME_SP_OFFSET(sp)
    sret
.Lguest_return:
    # Guest-anchor entries must undo the reserved HS anchor swap regardless of
    # whether hardware still reports SPV in the saved trap state. HS FS must
    # remain enabled while V=1 so the restored guest floating state is usable;
    # host-origin returns above retain their exact saved SSTATUS value.
    li t0, (3 << 13)
    csrs sstatus, t0
    addi t0, sp, TRAP_FRAME_SIZE
    csrw sscratch, t0
    ld t0, TRAP_FRAME_T0_OFFSET(sp)
    ld sp, TRAP_FRAME_SP_OFFSET(sp)
    sret
.size riscv64_trap_vector, . - riscv64_trap_vector

.global riscv64_call_trap_on_stack
.type riscv64_call_trap_on_stack, @function
riscv64_call_trap_on_stack:
    # a0 retains the trap-frame pointer, a1 is the aligned IRQ-stack top, and
    # a2 is an extern "C" callback that returns after dispatching the IRQ.
    addi a1, a1, -16
    sd sp, 0(a1)
    sd ra, 8(a1)
    mv sp, a1
    jalr a2
    ld t0, 0(sp)
    ld ra, 8(sp)
    mv sp, t0
    ret
.size riscv64_call_trap_on_stack, . - riscv64_call_trap_on_stack
