/*
 * SPDX-FileCopyrightText: 2026 roolrz
 * SPDX-License-Identifier: Apache-2.0
 */

#include "asm_constants.h"

    .section .text, "ax"
    .code64

    .global x86_64_vmlaunch
    .type x86_64_vmlaunch, @function
x86_64_vmlaunch:
    movq %rdi, %r11
    fxrstor64 VCPU_CONTEXT_FX_STATE_OFFSET(%r11)
    movq 8(%r11), %rbx
    movq 16(%r11), %rcx
    movq 24(%r11), %rdx
    movq 40(%r11), %rbp
    movq 48(%r11), %rsi
    movq 56(%r11), %rdi
    movq 64(%r11), %r8
    movq 72(%r11), %r9
    movq 80(%r11), %r10
    movq 96(%r11), %r12
    movq 104(%r11), %r13
    movq 112(%r11), %r14
    movq 120(%r11), %r15
    movq 0(%r11), %rax
    movq 88(%r11), %r11
    vmlaunch
    jmp x86_64_vmx_instruction_failure
    .size x86_64_vmlaunch, . - x86_64_vmlaunch

    .global x86_64_vmexit_entry
    .type x86_64_vmexit_entry, @function
x86_64_vmexit_entry:
    # Guest RFLAGS are held in the VMCS.  Rust and the SysV ABI require DF=0.
    cld
    pushq %rax
    pushq %rbx
    pushq %rcx
    pushq %rdx
    pushq %rbp
    pushq %rsi
    pushq %rdi
    pushq %r8
    pushq %r9
    pushq %r10
    pushq %r11
    pushq %r12
    pushq %r13
    pushq %r14
    pushq %r15
    subq $512, %rsp
    fxsave64 (%rsp)
    # Pass the register frame as an unborrowed owner pointer. The Rust
    # dispatcher scopes architecture references around typed policy calls.
    leaq 512(%rsp), %rdi
    call x86_64_vmexit_dispatch
    fxrstor64 (%rsp)
    addq $512, %rsp
    popq %r15
    popq %r14
    popq %r13
    popq %r12
    popq %r11
    popq %r10
    popq %r9
    popq %r8
    popq %rdi
    popq %rsi
    popq %rbp
    popq %rdx
    popq %rcx
    popq %rbx
    popq %rax
    vmresume
    jmp x86_64_vmx_instruction_failure
    .size x86_64_vmexit_entry, . - x86_64_vmexit_entry
