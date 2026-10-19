/* SPDX-FileCopyrightText: 2026 roolrz
 * SPDX-License-Identifier: Apache-2.0
 */

#include "native_abi_constants.h"

/* prepare_process installs real process-local capabilities in these slots of
 * the fixture's writable stack page. No selector value is part of this layout.
 * The first word remains the FP probe progress counter. */
#define NATIVE_TEST_EXECUTION_CONTEXT 16
#define NATIVE_TEST_SYSTEM_INFO 24
#define NATIVE_TEST_OBJECT_FACTORY 32
#define NATIVE_TEST_EVENT 40

#if defined(__aarch64__)
.macro native_arguments count
    .if \count < 1
    mov x0, #0
    .endif
    .if \count < 2
    mov x1, #0
    .endif
    .if \count < 3
    mov x2, #0
    .endif
    .if \count < 4
    mov x3, #0
    .endif
    .if \count < 5
    mov x4, #0
    .endif
    .if \count < 6
    mov x5, #0
    .endif
.endm
.macro native_invoke slot, operation, count=0
    native_arguments \count
    sub x8, sp, #4096
    ldr x8, [x8, #\slot]
    mov x6, #\operation
    svc #0
.endm

.macro native_publish_event
    sub x9, sp, #4096
    add x9, x9, #NATIVE_TEST_EVENT
    stlr x1, [x9]
.endm
#elif defined(__riscv)
.macro native_arguments count
    .if \count < 1
    li a0, 0
    .endif
    .if \count < 2
    li a1, 0
    .endif
    .if \count < 3
    li a2, 0
    .endif
    .if \count < 4
    li a3, 0
    .endif
    .if \count < 5
    li a4, 0
    .endif
    .if \count < 6
    li a5, 0
    .endif
.endm
.macro native_invoke slot, operation, count=0
    native_arguments \count
    li a7, -4096
    add a7, sp, a7
    ld a7, \slot(a7)
    li a6, \operation
    ecall
.endm
#endif
