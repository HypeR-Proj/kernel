/*
 * SPDX-FileCopyrightText: 2026 roolrz
 * SPDX-License-Identifier: Apache-2.0
 */

#include "capabilities.h"

/* Copied into independent EL0/EL1 mappings, never executed by the kernel. */
.section .rodata.fp_probes, "a"
.balign 4

.macro fp_program name, guest=0, wait=0, blocking=0
.global \name\()_start
.global \name\()_end
\name\()_start:
    .if \guest
    mov x19, #0x4453
    mov x23, x0                  // Shared guest progress counter IPA.
    mov x9, #(3 << 20)
    msr cpacr_el1, x9
    isb
    .else
    mov x19, #0x1235              // The Native fixture patches this seed.
    sub x23, sp, #4096            // Bottom of the fixture's writable stack.
    msr tpidr_el0, x19
    .if \blocking
    mov x0, #0
    native_invoke NATIVE_TEST_OBJECT_FACTORY, HYPER_NATIVE_OP_EVENT_CREATE, 1
    cbnz x0, .Lfail\@
    mov x25, x1
    native_publish_event
    sub x27, sp, #8192           // Untouched COW page, below the stack.
    ldrb w9, [x27]
    cmp w9, #0x5a
    b.ne .Lfail\@
    mov x8, x25
    mov x0, x27
    mov x1, #16                  // Complete HandleInfo record.
    mov x2, #0
    mov x3, #0
    mov x4, #0
    mov x5, #0
    mov x6, #HYPER_NATIVE_OP_HANDLE_GET_INFO // Must resolve COW copyout.
    svc #0
    cbnz x0, .Lfail\@
    cmp x1, #16
    b.ne .Lfail\@
    ldr w9, [x27]
    cmp w9, #1                   // ObjectKind::EVENT.
    b.ne .Lfail\@
    ldrb w9, [x27, #16]
    cmp w9, #0x5a                // Copyout must not overwrite the next byte.
    b.ne .Lfail\@
    .endif
    .endif
    .irp reg,v0.d[0],v0.d[1],v1.d[0],v1.d[1],v2.d[0],v2.d[1],v3.d[0],v3.d[1],v4.d[0],v4.d[1],v5.d[0],v5.d[1],v6.d[0],v6.d[1],v7.d[0],v7.d[1],v8.d[0],v8.d[1],v9.d[0],v9.d[1],v10.d[0],v10.d[1],v11.d[0],v11.d[1],v12.d[0],v12.d[1],v13.d[0],v13.d[1],v14.d[0],v14.d[1],v15.d[0],v15.d[1],v16.d[0],v16.d[1],v17.d[0],v17.d[1],v18.d[0],v18.d[1],v19.d[0],v19.d[1],v20.d[0],v20.d[1],v21.d[0],v21.d[1],v22.d[0],v22.d[1],v23.d[0],v23.d[1],v24.d[0],v24.d[1],v25.d[0],v25.d[1],v26.d[0],v26.d[1],v27.d[0],v27.d[1],v28.d[0],v28.d[1],v29.d[0],v29.d[1],v30.d[0],v30.d[1],v31.d[0],v31.d[1]
    umov x9, \reg
    cbnz x9, .Lfail\@
    .endr
    mrs x9, fpcr
    cbnz x9, .Lfail\@
    mrs x9, fpsr
    cbnz x9, .Lfail\@
    mvn x20, x19
    and x21, x19, #3
    lsl x21, x21, #22
    and x22, x19, #0x1f
    msr fpcr, x21
    msr fpsr, x22
    .irp reg,v0.2d,v1.2d,v2.2d,v3.2d,v4.2d,v5.2d,v6.2d,v7.2d,v8.2d,v9.2d,v10.2d,v11.2d,v12.2d,v13.2d,v14.2d,v15.2d,v16.2d,v17.2d,v18.2d,v19.2d,v20.2d,v21.2d,v22.2d,v23.2d,v24.2d,v25.2d,v26.2d,v27.2d,v28.2d,v29.2d,v30.2d,v31.2d
    dup \reg, x19
    .endr
    .irp reg,v0.d[1],v1.d[1],v2.d[1],v3.d[1],v4.d[1],v5.d[1],v6.d[1],v7.d[1],v8.d[1],v9.d[1],v10.d[1],v11.d[1],v12.d[1],v13.d[1],v14.d[1],v15.d[1],v16.d[1],v17.d[1],v18.d[1],v19.d[1],v20.d[1],v21.d[1],v22.d[1],v23.d[1],v24.d[1],v25.d[1],v26.d[1],v27.d[1],v28.d[1],v29.d[1],v30.d[1],v31.d[1]
    ins \reg, x20
    .endr
    mov x24, #0
.Lverify\@:
    .if !\guest
    mrs x9, tpidr_el0
    cmp x9, x19
    b.ne .Lfail\@
    .endif
    .irp reg,v0.d[0],v1.d[0],v2.d[0],v3.d[0],v4.d[0],v5.d[0],v6.d[0],v7.d[0],v8.d[0],v9.d[0],v10.d[0],v11.d[0],v12.d[0],v13.d[0],v14.d[0],v15.d[0],v16.d[0],v17.d[0],v18.d[0],v19.d[0],v20.d[0],v21.d[0],v22.d[0],v23.d[0],v24.d[0],v25.d[0],v26.d[0],v27.d[0],v28.d[0],v29.d[0],v30.d[0],v31.d[0]
    umov x9, \reg
    cmp x9, x19
    b.ne .Lfail\@
    .endr
    .irp reg,v0.d[1],v1.d[1],v2.d[1],v3.d[1],v4.d[1],v5.d[1],v6.d[1],v7.d[1],v8.d[1],v9.d[1],v10.d[1],v11.d[1],v12.d[1],v13.d[1],v14.d[1],v15.d[1],v16.d[1],v17.d[1],v18.d[1],v19.d[1],v20.d[1],v21.d[1],v22.d[1],v23.d[1],v24.d[1],v25.d[1],v26.d[1],v27.d[1],v28.d[1],v29.d[1],v30.d[1],v31.d[1]
    umov x9, \reg
    cmp x9, x20
    b.ne .Lfail\@
    .endr

    mrs x9, fpcr
    cmp x9, x21
    b.ne .Lfail\@
    mrs x9, fpsr
    cmp x9, x22
    b.ne .Lfail\@
    add x24, x24, #1
    stlr x24, [x23]
    .if \guest
    .if \wait
    mrs x9, cntfrq_el0
    mov x10, #100
    udiv x9, x9, x10
    msr cntv_tval_el0, x9
    mov x9, #1
    msr cntv_ctl_el0, x9
    isb
    wfi
    .endif
    .else
    .if \blocking
    mov x8, x25
    mov x0, #1
    mov x1, #-1
    mov x2, #0
    native_arguments 2
    mov x6, #HYPER_NATIVE_OP_OBJECT_WAIT_ONE // Controller migrates/wakes.
    svc #0
    cbnz x0, .Lfail\@
    cmp x1, #1
    b.ne .Lfail\@
    mov x8, x25
    mov x0, #1
    mov x1, #0
    native_arguments 2
    mov x6, #HYPER_NATIVE_OP_EVENT_SIGNAL // Clear Event for the next wait.
    svc #0
    cbnz x0, .Lfail\@
    .else
    native_invoke NATIVE_TEST_SYSTEM_INFO, HYPER_NATIVE_OP_ABI_QUERY
    mov x6, #-1
    svc #0                      // Unknown operation on the same capability.
    native_invoke NATIVE_TEST_EXECUTION_CONTEXT, HYPER_NATIVE_OP_THREAD_YIELD
    .endif
    .endif
    mov x9, #0x400000
.Lbusy\@:
    subs x9, x9, #1
    b.ne .Lbusy\@
    b .Lverify\@
.Lfail\@:
    mov x9, #-1
    stlr x9, [x23]
    brk #42
    b .Lfail\@
\name\()_end:
.endm

fp_program aarch64_native_fp_probe
fp_program aarch64_native_blocking_fp_probe, 0, 0, 1
fp_program aarch64_guest_fp_probe, 1
fp_program aarch64_guest_fp_wait_probe, 1, 1
