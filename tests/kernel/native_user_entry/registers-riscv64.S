/* SPDX-FileCopyrightText: 2026 roolrz
 * SPDX-License-Identifier: Apache-2.0
 */

#include "capabilities.h"
.option norvc
/* Raw user payload copied into an RX VMO by kernel self-tests. Every branch
 * is relative, so no kernel address or linker relocation enters userspace. */
.section .rodata.native_register_test, "a"
.balign 4
.global riscv64_native_register_test_start
.global riscv64_native_register_test_end
riscv64_native_register_test_start:
    ori s0, a0, 256
    addi gp, s0, 17
    addi tp, s0, 23
    addi t0, s0, 31
    addi s2, s0, 40
    addi s3, s0, 41
    addi s4, s0, 42
    addi s5, s0, 43
    addi s6, s0, 44
    addi s7, s0, 45
    addi s8, s0, 46
    addi s9, s0, 47
    addi s10, s0, 48
    addi s11, s0, 49
    li t1, 0x61
    fscsr t1
    addi t1, s0, 128
    fmv.d.x f0, t1
    addi t1, t1, 1
    fmv.d.x f1, t1
    addi t1, t1, 1
    fmv.d.x f2, t1
    addi t1, t1, 1
    fmv.d.x f3, t1
    addi t1, t1, 1
    fmv.d.x f4, t1
    addi t1, t1, 1
    fmv.d.x f5, t1
    addi t1, t1, 1
    fmv.d.x f6, t1
    addi t1, t1, 1
    fmv.d.x f7, t1
    addi t1, t1, 1
    fmv.d.x f8, t1
    addi t1, t1, 1
    fmv.d.x f9, t1
    addi t1, t1, 1
    fmv.d.x f10, t1
    addi t1, t1, 1
    fmv.d.x f11, t1
    addi t1, t1, 1
    fmv.d.x f12, t1
    addi t1, t1, 1
    fmv.d.x f13, t1
    addi t1, t1, 1
    fmv.d.x f14, t1
    addi t1, t1, 1
    fmv.d.x f15, t1
    addi t1, t1, 1
    fmv.d.x f16, t1
    addi t1, t1, 1
    fmv.d.x f17, t1
    addi t1, t1, 1
    fmv.d.x f18, t1
    addi t1, t1, 1
    fmv.d.x f19, t1
    addi t1, t1, 1
    fmv.d.x f20, t1
    addi t1, t1, 1
    fmv.d.x f21, t1
    addi t1, t1, 1
    fmv.d.x f22, t1
    addi t1, t1, 1
    fmv.d.x f23, t1
    addi t1, t1, 1
    fmv.d.x f24, t1
    addi t1, t1, 1
    fmv.d.x f25, t1
    addi t1, t1, 1
    fmv.d.x f26, t1
    addi t1, t1, 1
    fmv.d.x f27, t1
    addi t1, t1, 1
    fmv.d.x f28, t1
    addi t1, t1, 1
    fmv.d.x f29, t1
    addi t1, t1, 1
    fmv.d.x f30, t1
    addi t1, t1, 1
    fmv.d.x f31, t1
    addi t1, t1, 1
    li s1, 64
.Lregister_direct:
    native_invoke NATIVE_TEST_SYSTEM_INFO, HYPER_NATIVE_OP_ABI_QUERY
    li t3, HYPER_NATIVE_OP_ABI_QUERY
    li t4, 0
    bnez a0, .Lregister_failure
    bnez a1, .Lregister_failure
    li t1, 1
    bne a2, t1, .Lregister_failure
    jal ra, .Lregister_check
    addi s1, s1, -1
    bnez s1, .Lregister_direct
    li a6, -1
    addi a3, s0, 50
    addi a4, s0, 51
    addi a5, s0, 52
    ecall
    li t3, -1
    li t4, 1
    li t1, -4
    bne a0, t1, .Lregister_failure
    jal ra, .Lregister_check
    li a7, 0                   # Invalid capability, not an ambient selector.
    li a6, HYPER_NATIVE_OP_ABI_QUERY
    ecall
    li t1, -2
    bne a0, t1, .Lregister_failure
    bnez a1, .Lregister_failure
    bnez a2, .Lregister_failure
    li s1, 8
.Lregister_schedule:
    native_invoke NATIVE_TEST_EXECUTION_CONTEXT, HYPER_NATIVE_OP_THREAD_YIELD
    li t3, HYPER_NATIVE_OP_THREAD_YIELD
    li t4, 0
    bnez a0, .Lregister_failure
    li t2, 5000000
.Lregister_spin:
    addi t2, t2, -1
    bnez t2, .Lregister_spin
    jal ra, .Lregister_check
    addi s1, s1, -1
    bnez s1, .Lregister_schedule
    li a0, 0
    j .Lregister_exit
.Lregister_failure:
    li a0, -99
.Lregister_exit:
    native_invoke NATIVE_TEST_EXECUTION_CONTEXT, HYPER_NATIVE_OP_THREAD_EXIT, 1
    ebreak
.Lregister_check:
    addi t1, s0, 17
    bne gp, t1, .Lregister_failure
    addi t1, s0, 23
    bne tp, t1, .Lregister_failure
    addi t1, s0, 31
    bne t0, t1, .Lregister_failure
    addi t1, s0, 40
    bne s2, t1, .Lregister_failure
    addi t1, s0, 41
    bne s3, t1, .Lregister_failure
    addi t1, s0, 42
    bne s4, t1, .Lregister_failure
    addi t1, s0, 43
    bne s5, t1, .Lregister_failure
    addi t1, s0, 44
    bne s6, t1, .Lregister_failure
    addi t1, s0, 45
    bne s7, t1, .Lregister_failure
    addi t1, s0, 46
    bne s8, t1, .Lregister_failure
    addi t1, s0, 47
    bne s9, t1, .Lregister_failure
    addi t1, s0, 48
    bne s10, t1, .Lregister_failure
    addi t1, s0, 49
    bne s11, t1, .Lregister_failure
    beqz t4, .Lregister_zero_arguments
    addi t1, s0, 50
    bne a3, t1, .Lregister_failure
    addi t1, s0, 51
    bne a4, t1, .Lregister_failure
    addi t1, s0, 52
    bne a5, t1, .Lregister_failure
    j .Lregister_operation
.Lregister_zero_arguments:
    bnez a3, .Lregister_failure
    bnez a4, .Lregister_failure
    bnez a5, .Lregister_failure
.Lregister_operation:
    bne a6, t3, .Lregister_failure
    frcsr t1
    li t2, 0x61
    bne t1, t2, .Lregister_failure
    addi t2, s0, 128
    fmv.x.d t1, f0
    bne t1, t2, .Lregister_failure
    addi t2, t2, 1
    fmv.x.d t1, f1
    bne t1, t2, .Lregister_failure
    addi t2, t2, 1
    fmv.x.d t1, f2
    bne t1, t2, .Lregister_failure
    addi t2, t2, 1
    fmv.x.d t1, f3
    bne t1, t2, .Lregister_failure
    addi t2, t2, 1
    fmv.x.d t1, f4
    bne t1, t2, .Lregister_failure
    addi t2, t2, 1
    fmv.x.d t1, f5
    bne t1, t2, .Lregister_failure
    addi t2, t2, 1
    fmv.x.d t1, f6
    bne t1, t2, .Lregister_failure
    addi t2, t2, 1
    fmv.x.d t1, f7
    bne t1, t2, .Lregister_failure
    addi t2, t2, 1
    fmv.x.d t1, f8
    bne t1, t2, .Lregister_failure
    addi t2, t2, 1
    fmv.x.d t1, f9
    bne t1, t2, .Lregister_failure
    addi t2, t2, 1
    fmv.x.d t1, f10
    bne t1, t2, .Lregister_failure
    addi t2, t2, 1
    fmv.x.d t1, f11
    bne t1, t2, .Lregister_failure
    addi t2, t2, 1
    fmv.x.d t1, f12
    bne t1, t2, .Lregister_failure
    addi t2, t2, 1
    fmv.x.d t1, f13
    bne t1, t2, .Lregister_failure
    addi t2, t2, 1
    fmv.x.d t1, f14
    bne t1, t2, .Lregister_failure
    addi t2, t2, 1
    fmv.x.d t1, f15
    bne t1, t2, .Lregister_failure
    addi t2, t2, 1
    fmv.x.d t1, f16
    bne t1, t2, .Lregister_failure
    addi t2, t2, 1
    fmv.x.d t1, f17
    bne t1, t2, .Lregister_failure
    addi t2, t2, 1
    fmv.x.d t1, f18
    bne t1, t2, .Lregister_failure
    addi t2, t2, 1
    fmv.x.d t1, f19
    bne t1, t2, .Lregister_failure
    addi t2, t2, 1
    fmv.x.d t1, f20
    bne t1, t2, .Lregister_failure
    addi t2, t2, 1
    fmv.x.d t1, f21
    bne t1, t2, .Lregister_failure
    addi t2, t2, 1
    fmv.x.d t1, f22
    bne t1, t2, .Lregister_failure
    addi t2, t2, 1
    fmv.x.d t1, f23
    bne t1, t2, .Lregister_failure
    addi t2, t2, 1
    fmv.x.d t1, f24
    bne t1, t2, .Lregister_failure
    addi t2, t2, 1
    fmv.x.d t1, f25
    bne t1, t2, .Lregister_failure
    addi t2, t2, 1
    fmv.x.d t1, f26
    bne t1, t2, .Lregister_failure
    addi t2, t2, 1
    fmv.x.d t1, f27
    bne t1, t2, .Lregister_failure
    addi t2, t2, 1
    fmv.x.d t1, f28
    bne t1, t2, .Lregister_failure
    addi t2, t2, 1
    fmv.x.d t1, f29
    bne t1, t2, .Lregister_failure
    addi t2, t2, 1
    fmv.x.d t1, f30
    bne t1, t2, .Lregister_failure
    addi t2, t2, 1
    fmv.x.d t1, f31
    bne t1, t2, .Lregister_failure
    addi t2, t2, 1
    ret
riscv64_native_register_test_end:

.balign 4
.global riscv64_native_supervisor_load_start
.global riscv64_native_supervisor_load_end
riscv64_native_supervisor_load_start:
    ld t0, 0(a0)
    li a0, -99
    native_invoke NATIVE_TEST_EXECUTION_CONTEXT, HYPER_NATIVE_OP_THREAD_EXIT, 1
    ebreak
riscv64_native_supervisor_load_end:

.balign 4
.global riscv64_native_privileged_csr_start
.global riscv64_native_privileged_csr_end
riscv64_native_privileged_csr_start:
    csrr t0, satp
    li a0, -99
    native_invoke NATIVE_TEST_EXECUTION_CONTEXT, HYPER_NATIVE_OP_THREAD_EXIT, 1
    ebreak
riscv64_native_privileged_csr_end:

.balign 4
.global riscv64_native_integer_exit_start
.global riscv64_native_integer_exit_end
riscv64_native_integer_exit_start:
    li a0, 0
    native_invoke NATIVE_TEST_EXECUTION_CONTEXT, HYPER_NATIVE_OP_THREAD_EXIT, 1
    ebreak
riscv64_native_integer_exit_end:
