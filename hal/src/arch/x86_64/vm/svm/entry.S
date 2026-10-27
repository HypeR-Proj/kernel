/*
 * SPDX-FileCopyrightText: 2026 roolrz
 * SPDX-License-Identifier: Apache-2.0
 */

#include "asm_constants.h"

    .section .text, "ax"
    .code64

    # The VMCB physical address lives in RAX at VMRUN. Guest RAX, RSP and
    # RIP are transferred by hardware; all other GPRs are transferred here.
    .global x86_64_svm_run
    .type x86_64_svm_run, @function
x86_64_svm_run:
    subq $24, %rsp
    movq %rdi, 0(%rsp)       # VcpuContext
    movq %rsi, 8(%rsp)       # guest VMCB physical address
    movq %rdx, 16(%rsp)      # host-save physical address

    movq %rdx, %rax
    vmsave %rax
    # Keep GIF clear while host code temporarily sets IF for VMRUN's physical
    # interrupt mask snapshot.
    clgi

.Lsvm_resume:
    movq 0(%rsp), %r11
    fxrstor64 VCPU_CONTEXT_FX_STATE_OFFSET(%r11)

    movq 8(%rsp), %rax
    vmload %rax

    movq VCPU_CONTEXT_RBX_OFFSET(%r11), %rbx
    movq VCPU_CONTEXT_RCX_OFFSET(%r11), %rcx
    movq VCPU_CONTEXT_RDX_OFFSET(%r11), %rdx
    movq VCPU_CONTEXT_RBP_OFFSET(%r11), %rbp
    movq VCPU_CONTEXT_RSI_OFFSET(%r11), %rsi
    movq VCPU_CONTEXT_RDI_OFFSET(%r11), %rdi
    movq VCPU_CONTEXT_R8_OFFSET(%r11), %r8
    movq VCPU_CONTEXT_R9_OFFSET(%r11), %r9
    movq VCPU_CONTEXT_R10_OFFSET(%r11), %r10
    movq VCPU_CONTEXT_R12_OFFSET(%r11), %r12
    movq VCPU_CONTEXT_R13_OFFSET(%r11), %r13
    movq VCPU_CONTEXT_R14_OFFSET(%r11), %r14
    movq VCPU_CONTEXT_R15_OFFSET(%r11), %r15
    movq VCPU_CONTEXT_R11_OFFSET(%r11), %r11

    sti
    movq 8(%rsp), %rax
    vmrun %rax

    cli
    cld
    movq 0(%rsp), %rax
    movq %rbx, VCPU_CONTEXT_RBX_OFFSET(%rax)
    movq %rcx, VCPU_CONTEXT_RCX_OFFSET(%rax)
    movq %rdx, VCPU_CONTEXT_RDX_OFFSET(%rax)
    movq %rbp, VCPU_CONTEXT_RBP_OFFSET(%rax)
    movq %rsi, VCPU_CONTEXT_RSI_OFFSET(%rax)
    movq %rdi, VCPU_CONTEXT_RDI_OFFSET(%rax)
    movq %r8, VCPU_CONTEXT_R8_OFFSET(%rax)
    movq %r9, VCPU_CONTEXT_R9_OFFSET(%rax)
    movq %r10, VCPU_CONTEXT_R10_OFFSET(%rax)
    movq %r11, VCPU_CONTEXT_R11_OFFSET(%rax)
    movq %r12, VCPU_CONTEXT_R12_OFFSET(%rax)
    movq %r13, VCPU_CONTEXT_R13_OFFSET(%rax)
    movq %r14, VCPU_CONTEXT_R14_OFFSET(%rax)
    movq %r15, VCPU_CONTEXT_R15_OFFSET(%rax)
    fxsave64 VCPU_CONTEXT_FX_STATE_OFFSET(%rax)

    movq 8(%rsp), %rax
    vmsave %rax
    movq 16(%rsp), %rax
    vmload %rax

    # Pass the pinned context as an unborrowed owner pointer. The Rust
    # dispatcher creates architecture-local references only between typed
    # policy callbacks, which may borrow the containing vCPU execution.
    movq 0(%rsp), %rdi
    call x86_64_svm_exit_dispatch
    jmp .Lsvm_resume
    .size x86_64_svm_run, . - x86_64_svm_run
