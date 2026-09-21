/*
 * SPDX-FileCopyrightText: 2026 roolrz
 * SPDX-License-Identifier: Apache-2.0
 */

    .section .text, "ax"
    .code64
    .altmacro

    .macro X86_VECTOR number
    .balign 16
.Lvector_\number:
    .if (\number != 8) && (\number != 10) && (\number != 11) && (\number != 12) && (\number != 13) && (\number != 14) && (\number != 17) && (\number != 21) && (\number != 29) && (\number != 30)
        pushq $0
    .endif
        pushq $\number
        jmp x86_64_vector_common
    .endm

    .macro X86_VECTOR_POINTER number
        .long .Lvector_\number - x86_64_vector_stubs
    .endm

    .set vector_number, 0
    .rept 256
        X86_VECTOR %vector_number
        .set vector_number, vector_number + 1
    .endr

x86_64_vector_common:
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
    movq %rsp, %rdi
    movq %rsp, %r12

    /*
     * Rust's x86-64 ABI may use XMM registers in any interrupt handler. The
     * interrupted context therefore owns the complete legacy x87/SSE image,
     * including MXCSR. Keep the FXSAVE area below the exact integer frame so
     * ExceptionFrame layout and crash capture remain unchanged.
     */
    andq $-16, %rsp
    subq $512, %rsp
    fxsave64 (%rsp)
    call x86_64_vector_dispatch
    fxrstor64 (%rsp)
    movq %r12, %rsp
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
    addq $16, %rsp
    iretq

    .section .rodata, "a"
    .global x86_64_vector_stubs
    .type x86_64_vector_stubs, @object
    .balign 4
x86_64_vector_stubs:
    .set vector_number, 0
    .rept 256
        X86_VECTOR_POINTER %vector_number
        .set vector_number, vector_number + 1
    .endr
    .size x86_64_vector_stubs, . - x86_64_vector_stubs
