/* SPDX-FileCopyrightText: 2026 roolrz
 * SPDX-License-Identifier: Apache-2.0
 */

/* Included after the invocation fixture macros. The single mapped stack page
 * ends at SP: placing a record at SP - 24 makes the next record unmapped. */
.macro batch_call capacity=64, stride=24, options=0, reserved=0
    mov x8, x19
    mov x0, #0
    mov x1, x26
    mov x2, #\capacity
    mov x3, #\stride
    mov x4, #\options
    mov x5, #\reserved
    mov x6, #HYPER_NATIVE_OP_WAIT_SET_WAIT_BATCH
    svc #0
.endm

.macro batch_expect count, failure
    cbnz x0, \failure
    cmp x1, #\count
    b.ne \failure
    cbnz x2, \failure
.endm

.macro batch_invalid capacity, stride, options=0, reserved=0
    batch_call \capacity, \stride, \options, \reserved
    cmn x0, #1
    b.ne .Lbatch_bad_arguments
    cbnz x1, .Lbatch_bad_arguments
    cbnz x2, .Lbatch_bad_arguments
.endm

.macro batch_rearm registration
    mov x8, x19
    mov x0, \registration
    native_arguments 1
    mov x6, #HYPER_NATIVE_OP_WAIT_SET_REARM
    svc #0
    cbnz x0, .Lbatch_rearm_failed
.endm

.macro batch_add destination
    mov x8, x19
    mov x0, x23
    mov x1, #1
    native_arguments 2
    mov x6, #HYPER_NATIVE_OP_WAIT_SET_ADD
    svc #0
    cbnz x0, .Lbatch_setup_failed
    mov \destination, x1
.endm

program aarch64_native_wait_set_batch
    native_invoke NATIVE_TEST_OBJECT_FACTORY, HYPER_NATIVE_OP_EVENT_CREATE
    cbnz x0, .Lbatch_setup_failed
    mov x23, x1
    mov x8, x23
    mov x0, #0
    mov x1, #1
    native_arguments 2
    mov x6, #HYPER_NATIVE_OP_EVENT_SIGNAL
    svc #0
    cbnz x0, .Lbatch_setup_failed
    mov x0, #3
    native_invoke NATIVE_TEST_OBJECT_FACTORY, HYPER_NATIVE_OP_WAIT_SET_CREATE, 1
    cbnz x0, .Lbatch_setup_failed
    mov x19, x1

    /* Registering an asserted level gives deterministic FIFO order. */
    batch_add x20
    batch_add x21
    batch_add x22
    sub x26, sp, #128
    batch_invalid 0, 24
    batch_invalid 65, 24
    batch_invalid 1, 16
    batch_invalid 1, 25
    batch_invalid 1, 4104
    batch_invalid 1, 24, 1
    batch_invalid 1, 24, 0, 1

    /* A fault on the first record must report no progress and restore it. */
    mov x26, sp
    batch_call 1
    cbz x0, .Lbatch_first_fault
    cbnz x1, .Lbatch_first_fault
    cbnz x2, .Lbatch_first_fault

    /* Even a fault only in the unknown tail must retain the complete event. */
    sub x26, sp, #24
    batch_call 1, 32
    cbz x0, .Lbatch_tail_fault
    cbnz x1, .Lbatch_tail_fault
    cbnz x2, .Lbatch_tail_fault

    /* Capacity truncates the batch; unknown output-record tails are zeroed. */
    sub x26, sp, #128
    mov x9, #-1
    str x9, [x26, #24]
    str x9, [x26, #56]
    batch_call 2, 32
    batch_expect 2, .Lbatch_fifo_failed
    ldr x9, [x26]
    cmp x9, x20
    b.ne .Lbatch_fifo_failed
    ldr x9, [x26, #32]
    cmp x9, x21
    b.ne .Lbatch_fifo_failed
    ldr x9, [x26, #8]
    cmp x9, #1
    b.ne .Lbatch_fifo_failed
    ldr x9, [x26, #24]
    cbnz x9, .Lbatch_fifo_failed
    ldr x9, [x26, #56]
    cbnz x9, .Lbatch_fifo_failed

    /* The old operation retains its status-only result and consumes the tail. */
    mov x8, x19
    mov x0, #0
    mov x1, x26
    mov x2, #24
    native_arguments 3
    mov x6, #HYPER_NATIVE_OP_WAIT_SET_WAIT
    svc #0
    cbnz x0, .Lbatch_legacy_failed
    cbnz x1, .Lbatch_legacy_failed
    cbnz x2, .Lbatch_legacy_failed
    ldr x9, [x26]
    cmp x9, x22
    b.ne .Lbatch_legacy_failed
    batch_call
    cmn x0, #11
    b.ne .Lbatch_empty_failed
    cbnz x1, .Lbatch_empty_failed
    cbnz x2, .Lbatch_empty_failed

    /* A short batch returns available events despite its elapsed deadline. */
    batch_rearm x20
    batch_rearm x21
    batch_rearm x22
    mov x9, #-1
    str x9, [x26, #80]
    batch_call
    batch_expect 3, .Lbatch_short_failed
    ldr x9, [x26]
    cmp x9, x20
    b.ne .Lbatch_short_failed
    ldr x9, [x26, #24]
    cmp x9, x21
    b.ne .Lbatch_short_failed
    ldr x9, [x26, #48]
    cmp x9, x22
    b.ne .Lbatch_short_failed
    ldr x9, [x26, #80]
    cmn x9, #1
    b.ne .Lbatch_short_failed

    /* The second record is unmapped: commit only the first and retain the rest. */
    batch_rearm x20
    batch_rearm x21
    batch_rearm x22
    sub x26, sp, #24
    batch_call 2
    batch_expect 1, .Lbatch_prefix_failed
    ldr x9, [x26]
    cmp x9, x20
    b.ne .Lbatch_prefix_failed
    sub x26, sp, #128
    batch_call
    batch_expect 2, .Lbatch_restore_failed
    ldr x9, [x26]
    cmp x9, x21
    b.ne .Lbatch_restore_failed
    ldr x9, [x26, #24]
    cmp x9, x22
    b.ne .Lbatch_restore_failed

    /* Removing a queued subscription must not resurrect it in a later batch. */
    batch_rearm x20
    batch_rearm x21
    mov x8, x19
    mov x0, x20
    native_arguments 1
    mov x6, #HYPER_NATIVE_OP_WAIT_SET_REMOVE
    svc #0
    cbnz x0, .Lbatch_remove_failed
    batch_call
    batch_expect 1, .Lbatch_remove_failed
    ldr x9, [x26]
    cmp x9, x21
    b.ne .Lbatch_remove_failed

    mov x0, #0
    native_invoke NATIVE_TEST_EXECUTION_CONTEXT, HYPER_NATIVE_OP_THREAD_EXIT, 1
    brk #100
.Lbatch_setup_failed:
    brk #101
.Lbatch_bad_arguments:
    brk #102
.Lbatch_first_fault:
    brk #103
.Lbatch_fifo_failed:
    brk #104
.Lbatch_legacy_failed:
    brk #105
.Lbatch_empty_failed:
    brk #106
.Lbatch_rearm_failed:
    brk #107
.Lbatch_short_failed:
    brk #108
.Lbatch_prefix_failed:
    brk #109
.Lbatch_restore_failed:
    brk #110
.Lbatch_remove_failed:
    brk #111
.Lbatch_tail_fault:
    brk #112
end_program aarch64_native_wait_set_batch

.if aarch64_native_wait_set_batch_end - aarch64_native_wait_set_batch_start > 4096
.error "Native WaitSet batch fixture exceeds its executable page"
.endif
