/*
 * SPDX-FileCopyrightText: 2026 roolrz
 * SPDX-License-Identifier: Apache-2.0
 */

/*
 * Runtime AArch64 EL2 exception vectors.
 *
 * Each slot preserves the interrupted integer and SIMD state before entering
 * Rust. Frame layout constants are generated from registers.rs.
 */

#include "asm_constants.h"

.section .text.exceptions, "ax"

.macro VECTOR_SLOT number, uses_sp0=0
    // SP0 vectors arrive with SP selecting SP_EL0. Switch before touching the
    // stack so an untrusted lower-EL stack can never hold an EL2 frame.
    .if \uses_sp0
    msr     spsel, #1
    .endif
    sub     sp, sp, #EXCEPTION_FRAME_SIZE
    stp     x0, x1, [sp, #EXCEPTION_FRAME_X0_OFFSET]
    mov     x0, #\number
    b       .Lruntime_vector_common
    .if \uses_sp0
    .space  108
    .else
    .space  112
    .endif
.endm

.balign EXCEPTION_VECTOR_ALIGNMENT
.global aarch64_runtime_vectors
.type aarch64_runtime_vectors, %object
aarch64_runtime_vectors:
    VECTOR_SLOT EXCEPTION_VECTOR_CURRENT_SP0_SYNC, 1
    VECTOR_SLOT EXCEPTION_VECTOR_CURRENT_SP0_IRQ, 1
    VECTOR_SLOT EXCEPTION_VECTOR_CURRENT_SP0_FIQ, 1
    VECTOR_SLOT EXCEPTION_VECTOR_CURRENT_SP0_SERROR, 1
    VECTOR_SLOT EXCEPTION_VECTOR_CURRENT_SPX_SYNC
    VECTOR_SLOT EXCEPTION_VECTOR_CURRENT_SPX_IRQ
    VECTOR_SLOT EXCEPTION_VECTOR_CURRENT_SPX_FIQ
    VECTOR_SLOT EXCEPTION_VECTOR_CURRENT_SPX_SERROR
    VECTOR_SLOT EXCEPTION_VECTOR_LOWER_AARCH64_SYNC
    VECTOR_SLOT EXCEPTION_VECTOR_LOWER_AARCH64_IRQ
    VECTOR_SLOT EXCEPTION_VECTOR_LOWER_AARCH64_FIQ
    VECTOR_SLOT EXCEPTION_VECTOR_LOWER_AARCH64_SERROR
    VECTOR_SLOT EXCEPTION_VECTOR_LOWER_AARCH32_SYNC
    VECTOR_SLOT EXCEPTION_VECTOR_LOWER_AARCH32_IRQ
    VECTOR_SLOT EXCEPTION_VECTOR_LOWER_AARCH32_FIQ
    VECTOR_SLOT EXCEPTION_VECTOR_LOWER_AARCH32_SERROR
.size aarch64_runtime_vectors, . - aarch64_runtime_vectors

/*
 * Intentionally one linear exception path: every interrupted register must be
 * saved before the first Rust call and restored without introducing another
 * call frame. Splitting this sequence would clobber unsaved state or add an
 * exception-stack dependency to the most failure-sensitive path.
 */
.Lruntime_vector_common:
    stp     x2, x3, [sp, #(EXCEPTION_FRAME_X0_OFFSET + 16)]
    stp     x4, x5, [sp, #(EXCEPTION_FRAME_X0_OFFSET + 32)]
    stp     x6, x7, [sp, #(EXCEPTION_FRAME_X0_OFFSET + 48)]
    stp     x8, x9, [sp, #(EXCEPTION_FRAME_X0_OFFSET + 64)]
    stp     x10, x11, [sp, #(EXCEPTION_FRAME_X0_OFFSET + 80)]
    stp     x12, x13, [sp, #(EXCEPTION_FRAME_X0_OFFSET + 96)]
    stp     x14, x15, [sp, #(EXCEPTION_FRAME_X0_OFFSET + 112)]
    stp     x16, x17, [sp, #(EXCEPTION_FRAME_X0_OFFSET + 128)]
    stp     x18, x19, [sp, #(EXCEPTION_FRAME_X0_OFFSET + 144)]
    stp     x20, x21, [sp, #(EXCEPTION_FRAME_X0_OFFSET + 160)]
    stp     x22, x23, [sp, #(EXCEPTION_FRAME_X0_OFFSET + 176)]
    stp     x24, x25, [sp, #(EXCEPTION_FRAME_X0_OFFSET + 192)]
    stp     x26, x27, [sp, #(EXCEPTION_FRAME_X0_OFFSET + 208)]
    stp     x28, x29, [sp, #(EXCEPTION_FRAME_X0_OFFSET + 224)]
    str     x30, [sp, #EXCEPTION_FRAME_X30_OFFSET]

    mrs     x1, elr_el2
    str     x1, [sp, #EXCEPTION_FRAME_ELR_OFFSET]
    mrs     x1, spsr_el2
    str     x1, [sp, #EXCEPTION_FRAME_SPSR_OFFSET]
    mrs     x1, esr_el2
    str     x1, [sp, #EXCEPTION_FRAME_ESR_OFFSET]
    mrs     x1, far_el2
    str     x1, [sp, #EXCEPTION_FRAME_FAR_OFFSET]
    str     x0, [sp, #EXCEPTION_FRAME_VECTOR_OFFSET]
    mrs     x1, sp_el0
    str     x1, [sp, #EXCEPTION_FRAME_SP_EL0_OFFSET]
    mrs     x1, sp_el1
    str     x1, [sp, #EXCEPTION_FRAME_SP_EL1_OFFSET]
    mrs     x1, hcr_el2
    str     x1, [sp, #EXCEPTION_FRAME_RETURN_HCR_OFFSET]

    // A VHE guest exits with TGE clear. Switch back to the host EL2&0 regime
    // before any Rust handler accesses host-visible EL1/EL0 register aliases.
    ldr     x1, [sp, #EXCEPTION_FRAME_VECTOR_OFFSET]
    cmp     x1, #EXCEPTION_VECTOR_LOWER_AARCH64_SYNC
    b.lo    .Lhost_regime_active
    ldr     x1, [sp, #EXCEPTION_FRAME_RETURN_HCR_OFFSET]
    orr     x1, x1, #HCR_EL2_TGE
    msr     hcr_el2, x1
    isb
.Lhost_regime_active:

    stp     q0, q1, [sp, #(EXCEPTION_FRAME_SIMD_OFFSET + 0)]
    stp     q2, q3, [sp, #(EXCEPTION_FRAME_SIMD_OFFSET + 32)]
    stp     q4, q5, [sp, #(EXCEPTION_FRAME_SIMD_OFFSET + 64)]
    stp     q6, q7, [sp, #(EXCEPTION_FRAME_SIMD_OFFSET + 96)]
    stp     q8, q9, [sp, #(EXCEPTION_FRAME_SIMD_OFFSET + 128)]
    stp     q10, q11, [sp, #(EXCEPTION_FRAME_SIMD_OFFSET + 160)]
    stp     q12, q13, [sp, #(EXCEPTION_FRAME_SIMD_OFFSET + 192)]
    stp     q14, q15, [sp, #(EXCEPTION_FRAME_SIMD_OFFSET + 224)]
    stp     q16, q17, [sp, #(EXCEPTION_FRAME_SIMD_OFFSET + 256)]
    stp     q18, q19, [sp, #(EXCEPTION_FRAME_SIMD_OFFSET + 288)]
    stp     q20, q21, [sp, #(EXCEPTION_FRAME_SIMD_OFFSET + 320)]
    stp     q22, q23, [sp, #(EXCEPTION_FRAME_SIMD_OFFSET + 352)]
    stp     q24, q25, [sp, #(EXCEPTION_FRAME_SIMD_OFFSET + 384)]
    stp     q26, q27, [sp, #(EXCEPTION_FRAME_SIMD_OFFSET + 416)]
    stp     q28, q29, [sp, #(EXCEPTION_FRAME_SIMD_OFFSET + 448)]
    stp     q30, q31, [sp, #(EXCEPTION_FRAME_SIMD_OFFSET + 480)]
    mrs     x1, fpcr
    str     x1, [sp, #EXCEPTION_FRAME_FPCR_OFFSET]
    mrs     x1, fpsr
    str     x1, [sp, #EXCEPTION_FRAME_FPSR_OFFSET]

    // IRQ handlers run on a private per-CPU stack. The architectural frame
    // remains on the interrupted context so ERET and crash diagnostics retain
    // the exact original register image.
    mov     x19, sp
    ldr     x1, [x19, #EXCEPTION_FRAME_VECTOR_OFFSET]
    and     x1, x1, #3
    cmp     x1, #1
    b.ne    .Ldispatch_on_current_stack
    mrs     x1, tpidr_el2
    adrp    x2, AARCH64_IRQ_STACK_BOUNDS
    add     x2, x2, :lo12:AARCH64_IRQ_STACK_BOUNDS
    add     x2, x2, x1, lsl #4
    add     x4, x2, #8
    ldar    x3, [x4]
    ldr     x2, [x2]
    cbz     x3, .Ldispatch_on_current_stack
    cmp     x19, x2
    b.lo    .Lswitch_to_irq_stack
    cmp     x19, x3
    b.ls    .Ldispatch_on_current_stack
.Lswitch_to_irq_stack:
    mov     sp, x3
.Ldispatch_on_current_stack:
    mov     x0, x19
    bl      aarch64_exception_dispatch
    mov     sp, x19
    cbz     x0, .Lexception_tail_complete
    cmp     x0, #VECTOR_ACTION_POSTLUDE
    b.ne    .Lexception_unwind
    // The dispatcher has completed IRQ accounting and EOI. Scheduling is
    // permitted only after returning from the per-CPU IRQ stack to the
    // interrupted Thread's private stack; its complete frame remains pinned
    // above this call chain until the continuation resumes.
    cbz     x1, .Linvalid_vector_action
    blr     x1
    b       .Lexception_tail_complete
.Lexception_unwind:
    cmp     x0, #VECTOR_ACTION_UNWIND
    b.ne    .Linvalid_vector_action
    cbz     x1, .Linvalid_vector_action
    // Capture is complete and the lower-EL return world is closed. Branching
    // makes the non-return-to-vector contract explicit.
    br      x1
.Linvalid_vector_action:
    brk     #0
    b       .Linvalid_vector_action
.Lexception_tail_complete:

    // Restore the exact world captured before VHE entry switched into the host
    // register regime. A lower-EL vector alone cannot distinguish a guest from
    // Native EL0: guest return clears TGE, while Native return keeps it set.
    ldr     x1, [sp, #EXCEPTION_FRAME_RETURN_HCR_OFFSET]
    msr     hcr_el2, x1
    isb

    ldr     x1, [sp, #EXCEPTION_FRAME_ELR_OFFSET]
    msr     elr_el2, x1
    ldr     x1, [sp, #EXCEPTION_FRAME_SPSR_OFFSET]
    msr     spsr_el2, x1
    ldr     x1, [sp, #EXCEPTION_FRAME_SP_EL0_OFFSET]
    msr     sp_el0, x1
    ldr     x1, [sp, #EXCEPTION_FRAME_SP_EL1_OFFSET]
    msr     sp_el1, x1
    ldr     x1, [sp, #EXCEPTION_FRAME_FPCR_OFFSET]
    msr     fpcr, x1
    ldr     x1, [sp, #EXCEPTION_FRAME_FPSR_OFFSET]
    msr     fpsr, x1
    ldp     q0, q1, [sp, #(EXCEPTION_FRAME_SIMD_OFFSET + 0)]
    ldp     q2, q3, [sp, #(EXCEPTION_FRAME_SIMD_OFFSET + 32)]
    ldp     q4, q5, [sp, #(EXCEPTION_FRAME_SIMD_OFFSET + 64)]
    ldp     q6, q7, [sp, #(EXCEPTION_FRAME_SIMD_OFFSET + 96)]
    ldp     q8, q9, [sp, #(EXCEPTION_FRAME_SIMD_OFFSET + 128)]
    ldp     q10, q11, [sp, #(EXCEPTION_FRAME_SIMD_OFFSET + 160)]
    ldp     q12, q13, [sp, #(EXCEPTION_FRAME_SIMD_OFFSET + 192)]
    ldp     q14, q15, [sp, #(EXCEPTION_FRAME_SIMD_OFFSET + 224)]
    ldp     q16, q17, [sp, #(EXCEPTION_FRAME_SIMD_OFFSET + 256)]
    ldp     q18, q19, [sp, #(EXCEPTION_FRAME_SIMD_OFFSET + 288)]
    ldp     q20, q21, [sp, #(EXCEPTION_FRAME_SIMD_OFFSET + 320)]
    ldp     q22, q23, [sp, #(EXCEPTION_FRAME_SIMD_OFFSET + 352)]
    ldp     q24, q25, [sp, #(EXCEPTION_FRAME_SIMD_OFFSET + 384)]
    ldp     q26, q27, [sp, #(EXCEPTION_FRAME_SIMD_OFFSET + 416)]
    ldp     q28, q29, [sp, #(EXCEPTION_FRAME_SIMD_OFFSET + 448)]
    ldp     q30, q31, [sp, #(EXCEPTION_FRAME_SIMD_OFFSET + 480)]

    ldp     x2, x3, [sp, #(EXCEPTION_FRAME_X0_OFFSET + 16)]
    ldp     x4, x5, [sp, #(EXCEPTION_FRAME_X0_OFFSET + 32)]
    ldp     x6, x7, [sp, #(EXCEPTION_FRAME_X0_OFFSET + 48)]
    ldp     x8, x9, [sp, #(EXCEPTION_FRAME_X0_OFFSET + 64)]
    ldp     x10, x11, [sp, #(EXCEPTION_FRAME_X0_OFFSET + 80)]
    ldp     x12, x13, [sp, #(EXCEPTION_FRAME_X0_OFFSET + 96)]
    ldp     x14, x15, [sp, #(EXCEPTION_FRAME_X0_OFFSET + 112)]
    ldp     x16, x17, [sp, #(EXCEPTION_FRAME_X0_OFFSET + 128)]
    ldp     x18, x19, [sp, #(EXCEPTION_FRAME_X0_OFFSET + 144)]
    ldp     x20, x21, [sp, #(EXCEPTION_FRAME_X0_OFFSET + 160)]
    ldp     x22, x23, [sp, #(EXCEPTION_FRAME_X0_OFFSET + 176)]
    ldp     x24, x25, [sp, #(EXCEPTION_FRAME_X0_OFFSET + 192)]
    ldp     x26, x27, [sp, #(EXCEPTION_FRAME_X0_OFFSET + 208)]
    ldp     x28, x29, [sp, #(EXCEPTION_FRAME_X0_OFFSET + 224)]
    ldr     x30, [sp, #EXCEPTION_FRAME_X30_OFFSET]
    ldp     x0, x1, [sp, #EXCEPTION_FRAME_X0_OFFSET]
    add     sp, sp, #EXCEPTION_FRAME_SIZE
    eret
