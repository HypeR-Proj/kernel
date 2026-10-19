/* SPDX-FileCopyrightText: 2026 roolrz
 * SPDX-License-Identifier: Apache-2.0
 */

#include "capabilities.h"

/* These position-independent programs are copied into separate user VMOs.
 * Labels and generated operations keep branch and invocation encodings coupled
 * to the real ABI without reproducing an instruction encoder in the tests. */
.section .rodata.native_invocation_probes, "a"
.balign 4

.macro program name
.global \name\()_start
.global \name\()_end
\name\()_start:
.endm
.macro end_program name
\name\()_end:
.endm

program aarch64_native_integer
    mov x9, #64
1:
    native_invoke NATIVE_TEST_SYSTEM_INFO, HYPER_NATIVE_OP_ABI_QUERY
    cbnz x0, 3f
    cbnz x1, 3f
    cmp x2, #1
    b.ne 3f
    subs x9, x9, #1
    b.ne 1b
    mov x6, #-1                 // Valid capability, unknown object operation.
    svc #0
    cmn x0, #4
    b.ne 3f
    mov x8, #0                 // No implicit ambient target exists.
    mov x6, #HYPER_NATIVE_OP_ABI_QUERY
    svc #0
    cmn x0, #2
    b.ne 3f
    cbnz x1, 3f
    cbnz x2, 3f
    brk #0
3:
    brk #1
end_program aarch64_native_integer

program aarch64_native_thread_exit
    native_invoke NATIVE_TEST_EXECUTION_CONTEXT, HYPER_NATIVE_OP_THREAD_YIELD
    cbnz x0, 1f
    mov x0, #-17
    native_invoke NATIVE_TEST_EXECUTION_CONTEXT, HYPER_NATIVE_OP_THREAD_EXIT, 1
    brk #1
1:
    brk #2
end_program aarch64_native_thread_exit

program aarch64_native_process_exit
    mov x0, #23
    native_invoke NATIVE_TEST_EXECUTION_CONTEXT, HYPER_NATIVE_OP_PROCESS_EXIT, 1
    brk #1
end_program aarch64_native_process_exit

program aarch64_native_event_wait
    mov x0, #0
    native_invoke NATIVE_TEST_OBJECT_FACTORY, HYPER_NATIVE_OP_EVENT_CREATE, 1
    cbnz x0, 1f
    mov x19, x1
    native_publish_event
    mov x8, x19
    mov x0, #0
    mov x1, #1
    native_arguments 2
    mov x6, #HYPER_NATIVE_OP_EVENT_SIGNAL
    svc #0
    cbnz x0, 1f
    mov x8, x19
    mov x0, #1
    mov x1, #-1
    native_arguments 2
    mov x6, #HYPER_NATIVE_OP_OBJECT_WAIT_ONE
    svc #0
    cbnz x0, 1f
    cmp x1, #1
    b.ne 1f
    mov x0, #0
    native_invoke NATIVE_TEST_EXECUTION_CONTEXT, HYPER_NATIVE_OP_THREAD_EXIT, 1
    brk #1
1:
    brk #2
end_program aarch64_native_event_wait

program aarch64_native_cancelled_event_wait
    mov x0, #0
    native_invoke NATIVE_TEST_OBJECT_FACTORY, HYPER_NATIVE_OP_EVENT_CREATE, 1
    cbnz x0, 1f
    native_publish_event
    mov x8, x1
    mov x0, #1
    mov x1, #-1
    native_arguments 2
    mov x6, #HYPER_NATIVE_OP_OBJECT_WAIT_ONE
    svc #0
    brk #3                     // A cancelled Process must not return to EL0.
1:
    brk #2
end_program aarch64_native_cancelled_event_wait

program aarch64_native_rollover_wait
    mov x0, #0
    native_invoke NATIVE_TEST_OBJECT_FACTORY, HYPER_NATIVE_OP_EVENT_CREATE, 1
    cbnz x0, 1f
    native_publish_event
    mov x8, x1
    mov x0, #1
    mov x1, #-1
    native_arguments 2
    mov x6, #HYPER_NATIVE_OP_OBJECT_WAIT_ONE
    svc #0
    cbnz x0, 1f
    mov x0, #37
    native_invoke NATIVE_TEST_EXECUTION_CONTEXT, HYPER_NATIVE_OP_THREAD_EXIT, 1
    brk #1
1:
    brk #2
end_program aarch64_native_rollover_wait

program aarch64_native_atomic_wait
    sub x19, sp, #16
    str wzr, [x19]
    mov x0, x19
    mov x1, #1                 // Mismatch returns immediately.
    mov x2, #0
    mov x3, #0
    mov x4, #0
    mov x5, #0
    native_invoke NATIVE_TEST_EXECUTION_CONTEXT, HYPER_NATIVE_OP_ATOMIC_WAIT, 3
    cbnz x0, 1f
    mov x0, x19
    mov x1, #0
    mov x2, #0                 // Matching value, elapsed deadline.
    native_invoke NATIVE_TEST_EXECUTION_CONTEXT, HYPER_NATIVE_OP_ATOMIC_WAIT, 3
    cmn x0, #11
    b.ne 1f
    add x0, x19, #1            // Misaligned atomic word.
    mov x1, #0
    mov x2, #0
    native_invoke NATIVE_TEST_EXECUTION_CONTEXT, HYPER_NATIVE_OP_ATOMIC_WAIT, 3
    cmn x0, #1
    b.ne 1f
    mov x0, x19
    mov x1, #0
    mov x2, #-1                // Controller cancels this published wait.
    native_invoke NATIVE_TEST_EXECUTION_CONTEXT, HYPER_NATIVE_OP_ATOMIC_WAIT, 3
1:
    brk #4
end_program aarch64_native_atomic_wait
