/*
 * SPDX-FileCopyrightText: 2026 roolrz
 * SPDX-License-Identifier: Apache-2.0
 */

/* AAPCS64 cooperative kernel-thread context switch. */

#include "asm_constants.h"

.section .text.context, "ax"

.global aarch64_switch_context
.type aarch64_switch_context, %function
aarch64_switch_context:
    // x2 is the exact interrupt state owned by the outgoing continuation.
    // Save it before x2 becomes scratch for the remaining context fields.
    str     x2, [x0, #THREAD_CONTEXT_DAIF_OFFSET]
    stp     x19, x20, [x0, #(THREAD_CONTEXT_X19_OFFSET + 0)]
    stp     x21, x22, [x0, #(THREAD_CONTEXT_X19_OFFSET + 16)]
    stp     x23, x24, [x0, #(THREAD_CONTEXT_X19_OFFSET + 32)]
    stp     x25, x26, [x0, #(THREAD_CONTEXT_X19_OFFSET + 48)]
    stp     x27, x28, [x0, #(THREAD_CONTEXT_X19_OFFSET + 64)]
    stp     x29, x30, [x0, #THREAD_CONTEXT_X29_OFFSET]
    mov     x2, sp
    str     x2, [x0, #THREAD_CONTEXT_SP_OFFSET]
    stp     d8, d9, [x0, #(THREAD_CONTEXT_D8_OFFSET + 0)]
    stp     d10, d11, [x0, #(THREAD_CONTEXT_D8_OFFSET + 16)]
    stp     d12, d13, [x0, #(THREAD_CONTEXT_D8_OFFSET + 32)]
    stp     d14, d15, [x0, #(THREAD_CONTEXT_D8_OFFSET + 48)]
    mrs     x2, fpcr
    str     x2, [x0, #THREAD_CONTEXT_FPCR_OFFSET]
    mrs     x2, fpsr
    str     x2, [x0, #THREAD_CONTEXT_FPSR_OFFSET]
    // Run completion on the incoming stack before restoring its interrupt
    // state. x19 is already saved above and the callback preserves it.
    mov     x19, x1
    ldr     x2, [x19, #THREAD_CONTEXT_SP_OFFSET]
    mov     sp, x2
    mov     x0, x4
    blr     x3
    mov     x1, x19

    ldp     x19, x20, [x1, #(THREAD_CONTEXT_X19_OFFSET + 0)]
    ldp     x21, x22, [x1, #(THREAD_CONTEXT_X19_OFFSET + 16)]
    ldp     x23, x24, [x1, #(THREAD_CONTEXT_X19_OFFSET + 32)]
    ldp     x25, x26, [x1, #(THREAD_CONTEXT_X19_OFFSET + 48)]
    ldp     x27, x28, [x1, #(THREAD_CONTEXT_X19_OFFSET + 64)]
    ldp     x29, x30, [x1, #THREAD_CONTEXT_X29_OFFSET]
    ldp     d8, d9, [x1, #(THREAD_CONTEXT_D8_OFFSET + 0)]
    ldp     d10, d11, [x1, #(THREAD_CONTEXT_D8_OFFSET + 16)]
    ldp     d12, d13, [x1, #(THREAD_CONTEXT_D8_OFFSET + 32)]
    ldp     d14, d15, [x1, #(THREAD_CONTEXT_D8_OFFSET + 48)]
    ldr     x2, [x1, #THREAD_CONTEXT_FPCR_OFFSET]
    msr     fpcr, x2
    ldr     x2, [x1, #THREAD_CONTEXT_FPSR_OFFSET]
    msr     fpsr, x2
    ldr     x2, [x1, #THREAD_CONTEXT_DAIF_OFFSET]
    msr     daif, x2
    ret
.size aarch64_switch_context, . - aarch64_switch_context

.global aarch64_thread_trampoline
.type aarch64_thread_trampoline, %function
aarch64_thread_trampoline:
    mov     x0, x20
    blr     x19
    bl      kernel_thread_exit
    b       .
.size aarch64_thread_trampoline, . - aarch64_thread_trampoline

/*
 * Abandons the current call chain, refills one stack in place, and enters a
 * non-returning continuation at its clean top.
 *
 * x0 = mapped bottom, x1 = top, x2 = watermark word, x3 = canary,
 * x4 = callback, x5 = callback argument
 */
.global aarch64_reset_stack_and_enter
.type aarch64_reset_stack_and_enter, %function
aarch64_reset_stack_and_enter:
    mov     sp, x1
    mov     x29, xzr
    mov     x30, xzr
    str     x3, [x0], #8
.Lreset_stack_watermark:
    cmp     x0, x1
    b.hs    .Lreset_stack_enter
    str     x2, [x0], #8
    b       .Lreset_stack_watermark
.Lreset_stack_enter:
    mov     x0, x5
    blr     x4
.Lreset_stack_halt:
    msr     daifset, #0xf
    wfe
    b       .Lreset_stack_halt
.size aarch64_reset_stack_and_enter, . - aarch64_reset_stack_and_enter

/* Permanently transfers fatal handling onto this CPU's emergency stack. */
.global aarch64_run_on_emergency_stack
.type aarch64_run_on_emergency_stack, %function
aarch64_run_on_emergency_stack:
    mov     x19, x0
    mov     x20, x1
    mrs     x2, tpidr_el2
    adrp    x3, AARCH64_EMERGENCY_STACK_BOUNDS
    add     x3, x3, :lo12:AARCH64_EMERGENCY_STACK_BOUNDS
    add     x3, x3, x2, lsl #4
    add     x6, x3, #8
    ldar    x5, [x6]
    ldr     x4, [x3]
    cbz     x5, .Lemergency_current_stack
    mov     x6, sp
    cmp     x6, x4
    b.lo    .Lemergency_switch
    cmp     x6, x5
    b.ls    .Lemergency_current_stack
.Lemergency_switch:
    mov     sp, x5
    mov     x29, xzr
.Lemergency_current_stack:
    mov     x0, x20
    blr     x19
.Lemergency_halt:
    msr     daifset, #0xf
    wfe
    b       .Lemergency_halt
.size aarch64_run_on_emergency_stack, . - aarch64_run_on_emergency_stack

/* Captures a best-effort kernel register image for software panic paths. */
.global aarch64_capture_crash_context
.type aarch64_capture_crash_context, %function
aarch64_capture_crash_context:
    stp     x1, x2, [x0, #(CRASH_CONTEXT_X0_OFFSET + 8)]
    stp     x3, x4, [x0, #(CRASH_CONTEXT_X0_OFFSET + 24)]
    stp     x5, x6, [x0, #(CRASH_CONTEXT_X0_OFFSET + 40)]
    stp     x7, x8, [x0, #(CRASH_CONTEXT_X0_OFFSET + 56)]
    stp     x9, x10, [x0, #(CRASH_CONTEXT_X0_OFFSET + 72)]
    stp     x11, x12, [x0, #(CRASH_CONTEXT_X0_OFFSET + 88)]
    stp     x13, x14, [x0, #(CRASH_CONTEXT_X0_OFFSET + 104)]
    stp     x15, x16, [x0, #(CRASH_CONTEXT_X0_OFFSET + 120)]
    stp     x17, x18, [x0, #(CRASH_CONTEXT_X0_OFFSET + 136)]
    stp     x19, x20, [x0, #(CRASH_CONTEXT_X0_OFFSET + 152)]
    stp     x21, x22, [x0, #(CRASH_CONTEXT_X0_OFFSET + 168)]
    stp     x23, x24, [x0, #(CRASH_CONTEXT_X0_OFFSET + 184)]
    stp     x25, x26, [x0, #(CRASH_CONTEXT_X0_OFFSET + 200)]
    stp     x27, x28, [x0, #(CRASH_CONTEXT_X0_OFFSET + 216)]
    stp     x29, x30, [x0, #(CRASH_CONTEXT_X0_OFFSET + 232)]
    mov     x1, sp
    str     x1, [x0, #CRASH_CONTEXT_SP_OFFSET]
    str     x30, [x0, #CRASH_CONTEXT_PC_OFFSET]
    mrs     x1, nzcv
    mrs     x2, daif
    orr     x1, x1, x2
    mrs     x2, CurrentEL
    add     x3, x2, #1
    orr     x1, x1, x3
    str     x1, [x0, #CRASH_CONTEXT_PSTATE_OFFSET]
    mrs     x1, mpidr_el1
    str     x1, [x0, #CRASH_CONTEXT_MPIDR_OFFSET]
    str     x2, [x0, #CRASH_CONTEXT_CURRENT_EL_OFFSET]
    mrs     x1, daif
    str     x1, [x0, #CRASH_CONTEXT_DAIF_OFFSET]
    mrs     x1, sctlr_el2
    str     x1, [x0, #CRASH_CONTEXT_SCTLR_EL2_OFFSET]
    mrs     x1, tcr_el2
    str     x1, [x0, #CRASH_CONTEXT_TCR_EL2_OFFSET]
    mrs     x1, ttbr0_el2
    str     x1, [x0, #CRASH_CONTEXT_TTBR0_EL2_OFFSET]
    mrs     x2, hcr_el2
    // FEAT_VHE was admitted before runtime crash capture became reachable.
    mrs     x1, S3_4_C2_C0_1        // TTBR1_EL2
    str     x1, [x0, #CRASH_CONTEXT_TTBR1_EL2_OFFSET]
    mrs     x1, vbar_el2
    str     x1, [x0, #CRASH_CONTEXT_VBAR_EL2_OFFSET]
    str     x2, [x0, #CRASH_CONTEXT_HCR_EL2_OFFSET]
    ret
.size aarch64_capture_crash_context, . - aarch64_capture_crash_context

/*
 * Runs a native EL0 context as a call-like machine operation.
 *
 * A lower-EL exception which requires scheduling, blocking, or termination
 * copies the raw vector frame into the supplied context and branches to
 * aarch64_unwind_native_user. Never-blocking Native calls complete in the
 * vector and ERET directly without returning through this call frame.
 */
.global aarch64_run_native_user
.type aarch64_run_native_user, %function
aarch64_run_native_user:
    // ELR_EL2 and SPSR_EL2 become the pending user return state below. A
    // physical IRQ in that preparation window would overwrite both registers
    // with its EL2 return state, so retain the caller's exact mask and keep
    // local exceptions masked until the user exception has unwound.
    mrs     x2, daif
    msr     daifset, #0xf
    sub     sp, sp, #USER_RUN_FRAME_SIZE
    str     x2, [sp, #USER_RUN_FRAME_DAIF_OFFSET]
    mrs     x2, tpidr_el0
    str     x2, [sp, #USER_RUN_FRAME_TPIDR_EL0_OFFSET]
    mrs     x2, tpidrro_el0
    str     x2, [sp, #USER_RUN_FRAME_TPIDRRO_EL0_OFFSET]
    mrs     x2, mdcr_el2
    str     x2, [sp, #USER_RUN_FRAME_MDCR_EL2_OFFSET]
    orr     x2, x2, #MDCR_EL2_TPM
    msr     mdcr_el2, x2
    mrs     x2, cptr_el2
    str     x2, [sp, #USER_RUN_FRAME_CPTR_EL2_OFFSET]
    orr     x2, x2, #CPTR_EL2_TAM
    msr     cptr_el2, x2
    mrs     x2, sp_el0
    str     x2, [sp, #USER_RUN_FRAME_SP_EL0_OFFSET]
    stp     x19, x20, [sp, #(USER_RUN_FRAME_X19_OFFSET + 0)]
    stp     x21, x22, [sp, #(USER_RUN_FRAME_X19_OFFSET + 16)]
    stp     x23, x24, [sp, #(USER_RUN_FRAME_X19_OFFSET + 32)]
    stp     x25, x26, [sp, #(USER_RUN_FRAME_X19_OFFSET + 48)]
    stp     x27, x28, [sp, #(USER_RUN_FRAME_X19_OFFSET + 64)]
    stp     x29, x30, [sp, #USER_RUN_FRAME_X29_OFFSET]
    stp     d8, d9, [sp, #(USER_RUN_FRAME_D8_OFFSET + 0)]
    stp     d10, d11, [sp, #(USER_RUN_FRAME_D8_OFFSET + 16)]
    stp     d12, d13, [sp, #(USER_RUN_FRAME_D8_OFFSET + 32)]
    stp     d14, d15, [sp, #(USER_RUN_FRAME_D8_OFFSET + 48)]
    mrs     x1, fpcr
    str     x1, [sp, #USER_RUN_FRAME_FPCR_OFFSET]
    mrs     x1, fpsr
    str     x1, [sp, #USER_RUN_FRAME_FPSR_OFFSET]

    mov     x30, x0
    ldr     x1, [x30, #USER_CONTEXT_PC_OFFSET]
    msr     elr_el2, x1
    ldr     x1, [x30, #USER_CONTEXT_PSTATE_OFFSET]
    msr     spsr_el2, x1
    ldr     x1, [x30, #USER_CONTEXT_SP_OFFSET]
    msr     sp_el0, x1
    ldr     x1, [x30, #USER_CONTEXT_TPIDR_EL0_OFFSET]
    msr     tpidr_el0, x1
    ldr     x1, [x30, #USER_CONTEXT_TPIDRRO_EL0_OFFSET]
    msr     tpidrro_el0, x1
    ldr     x1, [x30, #USER_CONTEXT_FPCR_OFFSET]
    msr     fpcr, x1
    ldr     x1, [x30, #USER_CONTEXT_FPSR_OFFSET]
    msr     fpsr, x1
    ldp     q0, q1, [x30, #(USER_CONTEXT_SIMD_OFFSET + 0)]
    ldp     q2, q3, [x30, #(USER_CONTEXT_SIMD_OFFSET + 32)]
    ldp     q4, q5, [x30, #(USER_CONTEXT_SIMD_OFFSET + 64)]
    ldp     q6, q7, [x30, #(USER_CONTEXT_SIMD_OFFSET + 96)]
    ldp     q8, q9, [x30, #(USER_CONTEXT_SIMD_OFFSET + 128)]
    ldp     q10, q11, [x30, #(USER_CONTEXT_SIMD_OFFSET + 160)]
    ldp     q12, q13, [x30, #(USER_CONTEXT_SIMD_OFFSET + 192)]
    ldp     q14, q15, [x30, #(USER_CONTEXT_SIMD_OFFSET + 224)]
    ldp     q16, q17, [x30, #(USER_CONTEXT_SIMD_OFFSET + 256)]
    ldp     q18, q19, [x30, #(USER_CONTEXT_SIMD_OFFSET + 288)]
    ldp     q20, q21, [x30, #(USER_CONTEXT_SIMD_OFFSET + 320)]
    ldp     q22, q23, [x30, #(USER_CONTEXT_SIMD_OFFSET + 352)]
    ldp     q24, q25, [x30, #(USER_CONTEXT_SIMD_OFFSET + 384)]
    ldp     q26, q27, [x30, #(USER_CONTEXT_SIMD_OFFSET + 416)]
    ldp     q28, q29, [x30, #(USER_CONTEXT_SIMD_OFFSET + 448)]
    ldp     q30, q31, [x30, #(USER_CONTEXT_SIMD_OFFSET + 480)]

    // Keep the complete HCR value in the host frame. ESR_EL2 is not a
    // general-purpose scratch register: its reserved upper bits cannot
    // preserve HCR.E2H (bit 34), even if an emulator happens to retain them.
    // FAR_EL2 carries the full-width link register across the final switch.
    // Neither the context nor the stack is accessed after changing HCR.
    ldr     x1, [x30, #USER_CONTEXT_ENTRY_HCR_OFFSET]
    str     x1, [sp, #USER_RUN_FRAME_ENTRY_HCR_OFFSET]
    ldr     x1, [x30, #USER_CONTEXT_X30_OFFSET]
    msr     far_el2, x1
    ldp     x0, x1, [x30, #(USER_CONTEXT_X0_OFFSET + 0)]
    ldp     x2, x3, [x30, #(USER_CONTEXT_X0_OFFSET + 16)]
    ldp     x4, x5, [x30, #(USER_CONTEXT_X0_OFFSET + 32)]
    ldp     x6, x7, [x30, #(USER_CONTEXT_X0_OFFSET + 48)]
    ldp     x8, x9, [x30, #(USER_CONTEXT_X0_OFFSET + 64)]
    ldp     x10, x11, [x30, #(USER_CONTEXT_X0_OFFSET + 80)]
    ldp     x12, x13, [x30, #(USER_CONTEXT_X0_OFFSET + 96)]
    ldp     x14, x15, [x30, #(USER_CONTEXT_X0_OFFSET + 112)]
    ldp     x16, x17, [x30, #(USER_CONTEXT_X0_OFFSET + 128)]
    ldp     x18, x19, [x30, #(USER_CONTEXT_X0_OFFSET + 144)]
    ldp     x20, x21, [x30, #(USER_CONTEXT_X0_OFFSET + 160)]
    ldp     x22, x23, [x30, #(USER_CONTEXT_X0_OFFSET + 176)]
    ldp     x24, x25, [x30, #(USER_CONTEXT_X0_OFFSET + 192)]
    ldp     x26, x27, [x30, #(USER_CONTEXT_X0_OFFSET + 208)]
    ldp     x28, x29, [x30, #(USER_CONTEXT_X0_OFFSET + 224)]
    ldr     x30, [sp, #USER_RUN_FRAME_ENTRY_HCR_OFFSET]
    msr     hcr_el2, x30
    isb
    mrs     x30, far_el2
    eret
.size aarch64_run_native_user, . - aarch64_run_native_user

.global aarch64_unwind_native_user
.type aarch64_unwind_native_user, %function
aarch64_unwind_native_user:
    // Vector assembly restored SP to the native raw frame before invoking this
    // non-returning tail. The persistent UserContext already owns its contents.
    add     sp, sp, #EXCEPTION_FRAME_SIZE
    ldp     x19, x20, [sp, #(USER_RUN_FRAME_X19_OFFSET + 0)]
    ldp     x21, x22, [sp, #(USER_RUN_FRAME_X19_OFFSET + 16)]
    ldp     x23, x24, [sp, #(USER_RUN_FRAME_X19_OFFSET + 32)]
    ldp     x25, x26, [sp, #(USER_RUN_FRAME_X19_OFFSET + 48)]
    ldp     x27, x28, [sp, #(USER_RUN_FRAME_X19_OFFSET + 64)]
    ldp     x29, x30, [sp, #USER_RUN_FRAME_X29_OFFSET]
    ldp     d8, d9, [sp, #(USER_RUN_FRAME_D8_OFFSET + 0)]
    ldp     d10, d11, [sp, #(USER_RUN_FRAME_D8_OFFSET + 16)]
    ldp     d12, d13, [sp, #(USER_RUN_FRAME_D8_OFFSET + 32)]
    ldp     d14, d15, [sp, #(USER_RUN_FRAME_D8_OFFSET + 48)]
    ldr     x0, [sp, #USER_RUN_FRAME_FPCR_OFFSET]
    msr     fpcr, x0
    ldr     x0, [sp, #USER_RUN_FRAME_FPSR_OFFSET]
    msr     fpsr, x0
    ldr     x0, [sp, #USER_RUN_FRAME_TPIDR_EL0_OFFSET]
    msr     tpidr_el0, x0
    ldr     x0, [sp, #USER_RUN_FRAME_TPIDRRO_EL0_OFFSET]
    msr     tpidrro_el0, x0
    ldr     x0, [sp, #USER_RUN_FRAME_MDCR_EL2_OFFSET]
    msr     mdcr_el2, x0
    ldr     x0, [sp, #USER_RUN_FRAME_CPTR_EL2_OFFSET]
    msr     cptr_el2, x0
    ldr     x0, [sp, #USER_RUN_FRAME_SP_EL0_OFFSET]
    msr     sp_el0, x0
    isb
    ldr     x0, [sp, #USER_RUN_FRAME_DAIF_OFFSET]
    msr     daif, x0
    add     sp, sp, #USER_RUN_FRAME_SIZE
    ret
.size aarch64_unwind_native_user, . - aarch64_unwind_native_user

.global aarch64_run_guest
.type aarch64_run_guest, %function
aarch64_run_guest:
    // Local IRQs must stay masked until ERET. An IRQ in this trampoline would
    // replace the guest ELR_EL2/SPSR_EL2 with its host return state; vCPU
    // deactivation would then persist that host state as the guest context.
    // Keep the context pointer in x30 while the VHE host translation regime is
    // active. The heap may live in the host-only lower VA range, so no context
    // load is permitted after TGE is cleared.
    // This frame is the only legal target of a typed guest vector unwind. Save
    // the complete AAPCS64 host callee set before any guest register load.
    sub     sp, sp, #GUEST_RUN_FRAME_SIZE
    // Keep the Rust-computed guest HCR in this host frame until all guest
    // registers have been loaded and the context pointer is no longer needed.
    str     x1, [sp, #GUEST_RUN_FRAME_ENTRY_HCR_OFFSET]
    stp     x19, x20, [sp, #(GUEST_RUN_FRAME_X19_OFFSET + 0)]
    stp     x21, x22, [sp, #(GUEST_RUN_FRAME_X19_OFFSET + 16)]
    stp     x23, x24, [sp, #(GUEST_RUN_FRAME_X19_OFFSET + 32)]
    stp     x25, x26, [sp, #(GUEST_RUN_FRAME_X19_OFFSET + 48)]
    stp     x27, x28, [sp, #(GUEST_RUN_FRAME_X19_OFFSET + 64)]
    stp     x29, x30, [sp, #GUEST_RUN_FRAME_X29_OFFSET]
    stp     d8, d9, [sp, #(GUEST_RUN_FRAME_D8_OFFSET + 0)]
    stp     d10, d11, [sp, #(GUEST_RUN_FRAME_D8_OFFSET + 16)]
    stp     d12, d13, [sp, #(GUEST_RUN_FRAME_D8_OFFSET + 32)]
    stp     d14, d15, [sp, #(GUEST_RUN_FRAME_D8_OFFSET + 48)]
    mrs     x1, fpcr
    str     x1, [sp, #GUEST_RUN_FRAME_FPCR_OFFSET]
    mrs     x1, fpsr
    str     x1, [sp, #GUEST_RUN_FRAME_FPSR_OFFSET]

    mov     x30, x0

    // FAR_EL2 is overwritten by any subsequent fault, making it a safe
    // register-only shuttle for the guest link register across the final HCR
    // switch. This avoids dereferencing a host pointer in the guest regime.
    ldr     x1, [x30, #VCPU_CONTEXT_X30_OFFSET]
    msr     far_el2, x1
    ldr     x1, [x30, #VCPU_CONTEXT_PC_OFFSET]
    msr     elr_el2, x1
    ldr     x1, [x30, #VCPU_CONTEXT_PSTATE_OFFSET]
    msr     spsr_el2, x1
    ldr     x1, [x30, #VCPU_CONTEXT_FPCR_OFFSET]
    msr     fpcr, x1
    ldr     x1, [x30, #VCPU_CONTEXT_FPSR_OFFSET]
    msr     fpsr, x1
    ldp     q0, q1, [x30, #(VCPU_CONTEXT_SIMD_OFFSET + 0)]
    ldp     q2, q3, [x30, #(VCPU_CONTEXT_SIMD_OFFSET + 32)]
    ldp     q4, q5, [x30, #(VCPU_CONTEXT_SIMD_OFFSET + 64)]
    ldp     q6, q7, [x30, #(VCPU_CONTEXT_SIMD_OFFSET + 96)]
    ldp     q8, q9, [x30, #(VCPU_CONTEXT_SIMD_OFFSET + 128)]
    ldp     q10, q11, [x30, #(VCPU_CONTEXT_SIMD_OFFSET + 160)]
    ldp     q12, q13, [x30, #(VCPU_CONTEXT_SIMD_OFFSET + 192)]
    ldp     q14, q15, [x30, #(VCPU_CONTEXT_SIMD_OFFSET + 224)]
    ldp     q16, q17, [x30, #(VCPU_CONTEXT_SIMD_OFFSET + 256)]
    ldp     q18, q19, [x30, #(VCPU_CONTEXT_SIMD_OFFSET + 288)]
    ldp     q20, q21, [x30, #(VCPU_CONTEXT_SIMD_OFFSET + 320)]
    ldp     q22, q23, [x30, #(VCPU_CONTEXT_SIMD_OFFSET + 352)]
    ldp     q24, q25, [x30, #(VCPU_CONTEXT_SIMD_OFFSET + 384)]
    ldp     q26, q27, [x30, #(VCPU_CONTEXT_SIMD_OFFSET + 416)]
    ldp     q28, q29, [x30, #(VCPU_CONTEXT_SIMD_OFFSET + 448)]
    ldp     q30, q31, [x30, #(VCPU_CONTEXT_SIMD_OFFSET + 480)]
    ldp     x0, x1, [x30, #(VCPU_CONTEXT_X0_OFFSET + 0)]
    ldp     x2, x3, [x30, #(VCPU_CONTEXT_X0_OFFSET + 16)]
    ldp     x4, x5, [x30, #(VCPU_CONTEXT_X0_OFFSET + 32)]
    ldp     x6, x7, [x30, #(VCPU_CONTEXT_X0_OFFSET + 48)]
    ldp     x8, x9, [x30, #(VCPU_CONTEXT_X0_OFFSET + 64)]
    ldp     x10, x11, [x30, #(VCPU_CONTEXT_X0_OFFSET + 80)]
    ldp     x12, x13, [x30, #(VCPU_CONTEXT_X0_OFFSET + 96)]
    ldp     x14, x15, [x30, #(VCPU_CONTEXT_X0_OFFSET + 112)]
    ldp     x16, x17, [x30, #(VCPU_CONTEXT_X0_OFFSET + 128)]
    ldp     x18, x19, [x30, #(VCPU_CONTEXT_X0_OFFSET + 144)]
    ldp     x20, x21, [x30, #(VCPU_CONTEXT_X0_OFFSET + 160)]
    ldp     x22, x23, [x30, #(VCPU_CONTEXT_X0_OFFSET + 176)]
    ldp     x24, x25, [x30, #(VCPU_CONTEXT_X0_OFFSET + 192)]
    ldp     x26, x27, [x30, #(VCPU_CONTEXT_X0_OFFSET + 208)]
    ldp     x28, x29, [x30, #(VCPU_CONTEXT_X0_OFFSET + 224)]

    // Rust owns the guest-regime policy. Only register operations may follow
    // the HCR write until ERET.
    ldr     x30, [sp, #GUEST_RUN_FRAME_ENTRY_HCR_OFFSET]
    msr     hcr_el2, x30
    isb
    mrs     x30, far_el2
    eret
.size aarch64_run_guest, . - aarch64_run_guest

// Vector entry already copied the raw frame into VcpuContext and
// closed lower-EL ownership. This path never returns to vector restoration.
.global aarch64_unwind_guest
.type aarch64_unwind_guest, %function
aarch64_unwind_guest:
    add     sp, sp, #EXCEPTION_FRAME_SIZE
    ldp     x19, x20, [sp, #(GUEST_RUN_FRAME_X19_OFFSET + 0)]
    ldp     x21, x22, [sp, #(GUEST_RUN_FRAME_X19_OFFSET + 16)]
    ldp     x23, x24, [sp, #(GUEST_RUN_FRAME_X19_OFFSET + 32)]
    ldp     x25, x26, [sp, #(GUEST_RUN_FRAME_X19_OFFSET + 48)]
    ldp     x27, x28, [sp, #(GUEST_RUN_FRAME_X19_OFFSET + 64)]
    ldp     x29, x30, [sp, #GUEST_RUN_FRAME_X29_OFFSET]
    ldp     d8, d9, [sp, #(GUEST_RUN_FRAME_D8_OFFSET + 0)]
    ldp     d10, d11, [sp, #(GUEST_RUN_FRAME_D8_OFFSET + 16)]
    ldp     d12, d13, [sp, #(GUEST_RUN_FRAME_D8_OFFSET + 32)]
    ldp     d14, d15, [sp, #(GUEST_RUN_FRAME_D8_OFFSET + 48)]
    ldr     x0, [sp, #GUEST_RUN_FRAME_FPCR_OFFSET]
    msr     fpcr, x0
    ldr     x0, [sp, #GUEST_RUN_FRAME_FPSR_OFFSET]
    msr     fpsr, x0
    add     sp, sp, #GUEST_RUN_FRAME_SIZE
    mov     x0, #GUEST_RUN_RETURN_STOPPED
    ret
.size aarch64_unwind_guest, . - aarch64_unwind_guest
