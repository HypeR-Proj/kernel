/*
 * SPDX-FileCopyrightText: 2026 roolrz
 * SPDX-License-Identifier: Apache-2.0
 */

    .section .text.ap_trampoline, "ax"
    .code16
    .global x86_64_ap_trampoline_start
    .global x86_64_ap_trampoline_end
x86_64_ap_trampoline_start:
    cli
    cld
    xorw %ax, %ax
    movw %ax, %ds
    movw %ax, %es
    movw %ax, %ss
    movw $0x7ff0, %sp
    lgdtl %cs:(.Lgdt_descriptor - x86_64_ap_trampoline_start)
    movl %cr0, %eax
    orl $1, %eax
    movl %eax, %cr0
    ljmpl $0x10, $(0x8000 + .Lprotected - x86_64_ap_trampoline_start)

    .code32
.Lprotected:
    movw $0x18, %ax
    movw %ax, %ds
    movw %ax, %es
    movw %ax, %ss
    movl $1, %eax
    cpuid
    shrl $24, %ebx
    shll $6, %ebx
    addl $0x9000, %ebx
    movl %cr4, %eax
    orl $((1 << 5) | (1 << 9) | (1 << 10)), %eax
    movl %eax, %cr4
    movl 0(%ebx), %eax
    movl %eax, %cr3
    movl $0xc0000080, %ecx
    rdmsr
    orl $((1 << 8) | (1 << 11)), %eax
    wrmsr
    movl %cr0, %eax
    andl $~((1 << 2) | (1 << 3)), %eax
    orl $((1 << 31) | (1 << 16) | (1 << 5) | (1 << 1)), %eax
    movl %eax, %cr0
    ljmpl $0x20, $(0x8000 + .Llong_mode - x86_64_ap_trampoline_start)

    .code64
.Llong_mode:
    movw $0x18, %ax
    movw %ax, %ds
    movw %ax, %es
    movw %ax, %ss
    movq 16(%rbx), %rsp
    xorq %rbp, %rbp
    movq 24(%rbx), %rdi
    movq 32(%rbx), %rax
    jmp *%rax

    .balign 8
.Lgdt:
    .quad 0
    .quad 0
    .quad 0x00cf9a000000ffff
    .quad 0x00cf92000000ffff
    .quad 0x00af9a000000ffff
.Lgdt_end:
.Lgdt_descriptor:
    .word .Lgdt_end - .Lgdt - 1
    .long 0x8000 + .Lgdt - x86_64_ap_trampoline_start
x86_64_ap_trampoline_end:
