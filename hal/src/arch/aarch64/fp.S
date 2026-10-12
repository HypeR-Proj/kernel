/*
 * SPDX-FileCopyrightText: 2026 roolrz
 * SPDX-License-Identifier: Apache-2.0
 */

/* Audited FP-only leaves. All callers hold local exceptions masked. */
#include "asm_constants.h"
.section .text.context, "ax"

.macro fp_enable
    mrs x1, cptr_el2
    orr x2, x1, #CPTR_EL2_FPEN_ENABLED
    msr cptr_el2, x2
    isb
.endm

.macro fp_disable
    bic x1, x1, #CPTR_EL2_FPEN_ENABLED
    msr cptr_el2, x1
    isb
.endm

.macro fp_clear
    movi v0.16b, #0
    movi v1.16b, #0
    movi v2.16b, #0
    movi v3.16b, #0
    movi v4.16b, #0
    movi v5.16b, #0
    movi v6.16b, #0
    movi v7.16b, #0
    movi v8.16b, #0
    movi v9.16b, #0
    movi v10.16b, #0
    movi v11.16b, #0
    movi v12.16b, #0
    movi v13.16b, #0
    movi v14.16b, #0
    movi v15.16b, #0
    movi v16.16b, #0
    movi v17.16b, #0
    movi v18.16b, #0
    movi v19.16b, #0
    movi v20.16b, #0
    movi v21.16b, #0
    movi v22.16b, #0
    movi v23.16b, #0
    movi v24.16b, #0
    movi v25.16b, #0
    movi v26.16b, #0
    movi v27.16b, #0
    movi v28.16b, #0
    movi v29.16b, #0
    movi v30.16b, #0
    movi v31.16b, #0
    msr fpcr, xzr
    msr fpsr, xzr
.endm

/* CPU admission leaf: no stack, preserve all registers except x1 and x2. */
.global aarch64_fp_reset
.type aarch64_fp_reset, %function
aarch64_fp_reset:
    fp_enable
    fp_clear
    fp_disable
    ret
.size aarch64_fp_reset, . - aarch64_fp_reset

.global aarch64_fp_restore
.type aarch64_fp_restore, %function
aarch64_fp_restore:
    fp_enable
    ldp q0, q1, [x0, #0]
    ldp q2, q3, [x0, #32]
    ldp q4, q5, [x0, #64]
    ldp q6, q7, [x0, #96]
    ldp q8, q9, [x0, #128]
    ldp q10, q11, [x0, #160]
    ldp q12, q13, [x0, #192]
    ldp q14, q15, [x0, #224]
    ldp q16, q17, [x0, #256]
    ldp q18, q19, [x0, #288]
    ldp q20, q21, [x0, #320]
    ldp q22, q23, [x0, #352]
    ldp q24, q25, [x0, #384]
    ldp q26, q27, [x0, #416]
    ldp q28, q29, [x0, #448]
    ldp q30, q31, [x0, #480]
    ldr x2, [x0, #512]
    msr fpcr, x2
    ldr x2, [x0, #520]
    msr fpsr, x2
    fp_disable
    ret
.size aarch64_fp_restore, . - aarch64_fp_restore

.global aarch64_fp_save_and_clear
.type aarch64_fp_save_and_clear, %function
aarch64_fp_save_and_clear:
    fp_enable
    stp q0, q1, [x0, #0]
    stp q2, q3, [x0, #32]
    stp q4, q5, [x0, #64]
    stp q6, q7, [x0, #96]
    stp q8, q9, [x0, #128]
    stp q10, q11, [x0, #160]
    stp q12, q13, [x0, #192]
    stp q14, q15, [x0, #224]
    stp q16, q17, [x0, #256]
    stp q18, q19, [x0, #288]
    stp q20, q21, [x0, #320]
    stp q22, q23, [x0, #352]
    stp q24, q25, [x0, #384]
    stp q26, q27, [x0, #416]
    stp q28, q29, [x0, #448]
    stp q30, q31, [x0, #480]
    mrs x2, fpcr
    str x2, [x0, #512]
    mrs x2, fpsr
    str x2, [x0, #520]
    fp_clear
    fp_disable
    ret
.size aarch64_fp_save_and_clear, . - aarch64_fp_save_and_clear
