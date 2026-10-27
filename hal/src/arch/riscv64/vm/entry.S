/*
 * SPDX-FileCopyrightText: 2026 roolrz
 * SPDX-License-Identifier: Apache-2.0
 */

#include "asm_constants.h"

.section .text.guest, "ax"
.balign 4
.global riscv64_enter_guest
.type riscv64_enter_guest, @function
riscv64_enter_guest:
    csrci sstatus, SSTATUS_SIE
    mv t6, a0
    addi sp, sp, -GUEST_HS_ANCHOR_SIZE
    sd ra, GUEST_HS_ANCHOR_RA_OFFSET(sp)
    sd s0, GUEST_HS_ANCHOR_S0_OFFSET + 0(sp)
    sd s1, GUEST_HS_ANCHOR_S0_OFFSET + 8(sp)
    sd s2, GUEST_HS_ANCHOR_S0_OFFSET + 16(sp)
    sd s3, GUEST_HS_ANCHOR_S0_OFFSET + 24(sp)
    sd s4, GUEST_HS_ANCHOR_S0_OFFSET + 32(sp)
    sd s5, GUEST_HS_ANCHOR_S0_OFFSET + 40(sp)
    sd s6, GUEST_HS_ANCHOR_S0_OFFSET + 48(sp)
    sd s7, GUEST_HS_ANCHOR_S0_OFFSET + 56(sp)
    sd s8, GUEST_HS_ANCHOR_S0_OFFSET + 64(sp)
    sd s9, GUEST_HS_ANCHOR_S0_OFFSET + 72(sp)
    sd s10, GUEST_HS_ANCHOR_S0_OFFSET + 80(sp)
    sd s11, GUEST_HS_ANCHOR_S0_OFFSET + 88(sp)
    sd gp, GUEST_HS_ANCHOR_GP_OFFSET(sp)
    sd tp, GUEST_HS_ANCHOR_TP_OFFSET(sp)
    sd t6, GUEST_HS_ANCHOR_CONTEXT_OFFSET(sp)
    # Rust restored this run's bank with FS disabled; gate stays closed until SRET.
    li t0, SSTATUS_FS_MASK
    csrc sstatus, t0
    csrr t0, scounteren
    sd t0, GUEST_HS_ANCHOR_SCOUNTEREN_OFFSET(sp)
    csrr t0, senvcfg
    sd t0, GUEST_HS_ANCHOR_SENVCFG_OFFSET(sp)

    # Delegate exceptions handled by a normal supervisor kernel. Guest
    # supervisor ecalls and guest-page faults remain in HS mode for SBI and
    # stage-2 emulation. Install delegation BEFORE the VS bank: VSIE bits
    # are read-only zero until their HIDELEG gates are enabled. In particular,
    # migration onto a hart which has never run a guest must retain STIE.
    li t0, HEDELEG_GUEST_VALUE
    csrw hedeleg, t0
    li t0, HIDELEG_GUEST_VALUE
    csrw hideleg, t0
    li t0, HCOUNTEREN_GUEST_VALUE
    csrw hcounteren, t0

    ld t0, VCPU_VSSTATUS_OFFSET(t6)
    csrw vsstatus, t0
    ld t0, VCPU_VSIE_OFFSET(t6)
    csrw vsie, t0
    ld t0, VCPU_VSTVEC_OFFSET(t6)
    csrw vstvec, t0
    ld t0, VCPU_VSSCRATCH_OFFSET(t6)
    csrw vsscratch, t0
    ld t0, VCPU_VSEPC_OFFSET(t6)
    csrw vsepc, t0
    ld t0, VCPU_VSCAUSE_OFFSET(t6)
    csrw vscause, t0
    ld t0, VCPU_VSTVAL_OFFSET(t6)
    csrw vstval, t0
    li t0, HVIP_GUEST_MASK
    csrc hvip, t0
    ld t1, VCPU_HVIP_OFFSET(t6)
    and t1, t1, t0
    csrs hvip, t1
    ld t0, VCPU_VSATP_OFFSET(t6)
    csrw vsatp, t0
    hfence.vvma zero, zero
    ld t0, VCPU_SCOUNTEREN_OFFSET(t6)
    csrw scounteren, t0
    ld t0, VCPU_SENVCFG_OFFSET(t6)
    csrw senvcfg, t0
    li t0, (1 << 63)
    csrs henvcfg, t0
    ld t0, VCPU_VSTIMECMP_OFFSET(t6)
    csrw 0x24d, t0

    # SPV=1 selects virtualized execution and SPVP=1 enters VS mode.
    csrr t0, hstatus
    # Trap guest WFI so HS retains control of physical idle state. Keep VTSR
    # and VTVM clear, while setting VTW.
    li t1, HSTATUS_GUEST_CLEAR
    not t1, t1
    and t0, t0, t1
    li t1, HSTATUS_GUEST_SET
    or t0, t0, t1
    li t1, (1 << 8)
    csrc hstatus, t1
    not t1, t1
    and t0, t0, t1
    ld t1, VCPU_SUPERVISOR_OFFSET(t6)
    slli t1, t1, 8
    or t0, t0, t1
    csrw hstatus, t0
    # SPP selects supervisor privilege for the HS sret; SPV above selects the
    # virtualized execution domain.
    li t0, SSTATUS_GUEST_SPP
    csrc sstatus, t0
    ld t0, VCPU_SUPERVISOR_OFFSET(t6)
    slli t0, t0, 8
    csrs sstatus, t0
    ld t0, VCPU_PC_OFFSET(t6)
    csrw sepc, t0

    # Preserve the reserved HS anchor for trap entry before loading guest
    # registers. SPIE carries the desired HS interrupt-enabled state across the
    # initial sret into VS mode.
    csrw sscratch, sp
    li t0, (SSTATUS_SPIE | SSTATUS_FS_CLEAN)
    csrs sstatus, t0
    ld ra, 8(t6)
    ld sp, 16(t6)
    ld gp, 24(t6)
    ld tp, 32(t6)
    ld t0, 40(t6)
    ld t1, 48(t6)
    ld t2, 56(t6)
    ld s0, 64(t6)
    ld s1, 72(t6)
    ld a0, 80(t6)
    ld a1, 88(t6)
    ld a2, 96(t6)
    ld a3, 104(t6)
    ld a4, 112(t6)
    ld a5, 120(t6)
    ld a6, 128(t6)
    ld a7, 136(t6)
    ld s2, 144(t6)
    ld s3, 152(t6)
    ld s4, 160(t6)
    ld s5, 168(t6)
    ld s6, 176(t6)
    ld s7, 184(t6)
    ld s8, 192(t6)
    ld s9, 200(t6)
    ld s10, 208(t6)
    ld s11, 216(t6)
    ld t3, 224(t6)
    ld t4, 232(t6)
    ld t5, 240(t6)
    ld t6, 248(t6)
    sret
.size riscv64_enter_guest, . - riscv64_enter_guest


.global riscv64_activate_stage2
.type riscv64_activate_stage2, @function
riscv64_activate_stage2:
    # The old VS root must not remain live while HGATP changes: speculative
    # VS-stage walks could otherwise populate translations under the new VMID.
    # HFENCE.GVMA itself orders earlier visible page-table stores before later
    # implicit G-stage reads, so a separate full memory FENCE is unnecessary.
    csrw vsatp, zero
    hfence.vvma zero, zero
    csrw hgatp, a0
    hfence.gvma zero, zero
    ret
.size riscv64_activate_stage2, . - riscv64_activate_stage2

.global riscv64_invalidate_stage2_page
.type riscv64_invalidate_stage2_page, @function
riscv64_invalidate_stage2_page:
    # HFENCE.GVMA supplies the architectural page-table publication ordering;
    # the calling ownership protocol makes any remote stores visible locally.
    hfence.gvma a0, a1
    ret
.size riscv64_invalidate_stage2_page, . - riscv64_invalidate_stage2_page



# Fault-contained instruction parcel fetch. The caller holds the active HGATP
# lease and masks IRQs. a0=guest VA, a1=context, a2=result, a3=saved hstatus.
# The guest bank is reinstated only inside this assembly region; no Rust runs
# before every host CSR has been restored. Result: value/scause/stval/htval/htinst.
.balign 4
.global riscv64_fetch_guest_parcel
.type riscv64_fetch_guest_parcel, @function
riscv64_fetch_guest_parcel:
    addi sp, sp, -96
    sd ra, 0(sp)
    csrr t0, stvec
    sd t0, 8(sp)
    csrr t0, hstatus
    sd t0, 16(sp)
    csrr t0, vsatp
    sd t0, 24(sp)
    csrr t0, vsstatus
    sd t0, 32(sp)
    csrr t0, sstatus
    sd t0, 40(sp)
    csrr t0, sepc
    sd t0, 48(sp)
    csrr t0, scause
    sd t0, 56(sp)
    csrr t0, stval
    sd t0, 64(sp)
    csrr t0, htval
    sd t0, 72(sp)
    csrr t0, htinst
    sd t0, 80(sp)
    sd a2, 88(sp)
    li t0, ((1 << 7) | (1 << 9))
    not t0, t0
    and a3, a3, t0
    csrw hstatus, a3
    ld t0, VCPU_VSSTATUS_OFFSET(a1)
    # Instruction fetch from VS must never gain access to a U page via SUM.
    li t1, (1 << 18)
    not t1, t1
    and t0, t0, t1
    csrw vsstatus, t0
    ld t0, VCPU_VSATP_OFFSET(a1)
    csrw vsatp, t0
    hfence.vvma zero, zero
    la t0, .Lparcel_fault
    csrw stvec, t0
.Lparcel_load:
    hlvx.hu t0, (a0)
    sd t0, 0(a2)
    li a0, 0
    j .Lparcel_restore
.balign 4
.Lparcel_fault:
    csrr t0, sepc
    la t1, .Lparcel_load
    bne t0, t1, .Lparcel_unexpected
    ld t1, 88(sp)
    csrr t0, scause
    sd t0, 8(t1)
    csrr t0, stval
    sd t0, 16(t1)
    csrr t0, htval
    sd t0, 24(t1)
    csrr t0, htinst
    sd t0, 32(t1)
    li a0, 1
    # Stay in HS: no SRET is necessary, since restoration below returns to the
    # original masked caller and restores its complete interrupted CSR image.
.Lparcel_restore:
    ld t0, 8(sp)
    csrw stvec, t0
    ld t0, 24(sp)
    csrw vsatp, t0
    hfence.vvma zero, zero
    ld t0, 32(sp)
    csrw vsstatus, t0
    ld t0, 16(sp)
    csrw hstatus, t0
    ld t0, 48(sp)
    csrw sepc, t0
    ld t0, 56(sp)
    csrw scause, t0
    ld t0, 64(sp)
    csrw stval, t0
    ld t0, 72(sp)
    csrw htval, t0
    ld t0, 80(sp)
    csrw htinst, t0
    ld t0, 40(sp)
    csrw sstatus, t0
    ld ra, 0(sp)
    addi sp, sp, 96
    ret
.Lparcel_unexpected:
    # A fault outside the single authorized access is a kernel defect.
    ld t0, 8(sp)
    csrw stvec, t0
    jr t0
.size riscv64_fetch_guest_parcel, . - riscv64_fetch_guest_parcel
