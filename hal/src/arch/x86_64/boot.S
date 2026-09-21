/*
 * SPDX-FileCopyrightText: 2026 roolrz
 * SPDX-License-Identifier: Apache-2.0
 */

#include "asm_constants.h"

    .section .text.boot, "ax"
    .code32
    .global x86_64_protected_entry
    .type x86_64_protected_entry, @function
x86_64_protected_entry:
    rdtsc
    movl %eax, %ebp
    movl %edx, %edi
    cli
    cld
    movw $0x18, %ax
    movw %ax, %ds
    movw %ax, %es
    movw %ax, %ss

    call 1f
1:  popl %ebx
    subl $(1b - x86_64_protected_entry), %ebx
    movl %ebp, (.Lboot_counter_ticks - x86_64_protected_entry)(%ebx)
    movl %edi, (.Lboot_counter_ticks + 4 - x86_64_protected_entry)(%ebx)

    leal (__boot_pml4 - x86_64_protected_entry)(%ebx), %edi
    xorl %eax, %eax
    movl $(3 * 4096 / 4), %ecx
    rep stosl

    leal (__boot_pdpt - x86_64_protected_entry)(%ebx), %eax
    orl $(PAGE_PRESENT | PAGE_WRITE), %eax
    movl %eax, (__boot_pml4 - x86_64_protected_entry)(%ebx)
    leal (__boot_pd - x86_64_protected_entry)(%ebx), %eax
    orl $(PAGE_PRESENT | PAGE_WRITE), %eax
    movl %eax, (__boot_pdpt - x86_64_protected_entry)(%ebx)

    leal (__boot_pd - x86_64_protected_entry)(%ebx), %edi
    xorl %ecx, %ecx
.Lmap_boot_gib:
    movl %ecx, %eax
    shll $21, %eax
    orl $(PAGE_PRESENT | PAGE_WRITE | PAGE_LARGE), %eax
    movl %eax, (%edi,%ecx,8)
    incl %ecx
    cmpl $512, %ecx
    jne .Lmap_boot_gib

    movl %cr4, %eax
    orl $((1 << 5) | (1 << 9) | (1 << 10)), %eax /* PAE | OSFXSR | OSXMMEXCPT */
    movl %eax, %cr4
    leal (__boot_pml4 - x86_64_protected_entry)(%ebx), %eax
    movl %eax, %cr3

    movl $0xc0000080, %ecx       /* IA32_EFER */
    rdmsr
    orl $((1 << 8) | (1 << 11)), %eax /* LME | NXE */
    wrmsr

    leal (.Llong_mode_entry - x86_64_protected_entry)(%ebx), %eax
    movl %eax, (.Llong_pointer - x86_64_protected_entry)(%ebx)
    movw $0x20, (.Llong_pointer + 4 - x86_64_protected_entry)(%ebx)
    movl %cr0, %eax
    andl $~((1 << 2) | (1 << 3)), %eax /* clear EM | TS */
    orl $((1 << 31) | (1 << 16) | (1 << 5) | (1 << 1)), %eax /* PG | WP | NE | MP */
    movl %eax, %cr0
    ljmpl *(.Llong_pointer - x86_64_protected_entry)(%ebx)

    .balign 8
.Llong_pointer:
    .long 0
    .word 0

    .code64
.Llong_mode_entry:
    movw $0x18, %ax
    movw %ax, %ds
    movw %ax, %es
    movw %ax, %ss
    movl %esi, %r12d
    leaq __image_start(%rip), %r13

    call .Lapply_load_relocations
    call .Lclear_bss

    movq .Lboot_counter_ticks(%rip), %r14
    leaq __boot_stack_top(%rip), %rsp
    andq $-16, %rsp
    xorq %rbp, %rbp
    movq %r12, %rdi
    movq %r14, %rsi
    movq %rsi, %rdx
    xorq %rsi, %rsi
    call hyper_bootstrap
.Lhalt:
    cli
    hlt
    jmp .Lhalt

    .pushsection .data.boot, "aw"
    .balign 8
.Lboot_counter_ticks:
    .quad 0
    .popsection

.Lapply_load_relocations:
    leaq __rela_dyn_start(%rip), %rsi
    leaq __rela_dyn_end(%rip), %rdi
.Lrela_load_loop:
    cmpq %rdi, %rsi
    jae .Lrela_load_done
    movq 0(%rsi), %rax
    movq 8(%rsi), %rcx
    movq 16(%rsi), %rdx
    cmpl $R_X86_64_RELATIVE, %ecx
    jne .Lhalt
    addq %r13, %rax
    addq %r13, %rdx
    movq %rdx, (%rax)
    addq $24, %rsi
    jmp .Lrela_load_loop
.Lrela_load_done:
    leaq __relr_dyn_start(%rip), %rsi
    leaq __relr_dyn_end(%rip), %rdi
    xorq %rbx, %rbx
.Lrelr_load_loop:
    cmpq %rdi, %rsi
    jae .Lrelr_load_done
    movq (%rsi), %rax
    addq $8, %rsi
    testq $1, %rax
    jnz .Lrelr_load_bitmap
    leaq (%r13,%rax), %rbx
    addq %r13, (%rbx)
    addq $8, %rbx
    jmp .Lrelr_load_loop
.Lrelr_load_bitmap:
    testq %rbx, %rbx
    jz .Lhalt
    shrq $1, %rax
    movl $63, %ecx
.Lrelr_load_bitmap_loop:
    testq $1, %rax
    jz .Lrelr_load_bitmap_next
    addq %r13, (%rbx)
.Lrelr_load_bitmap_next:
    addq $8, %rbx
    shrq $1, %rax
    decl %ecx
    jnz .Lrelr_load_bitmap_loop
    jmp .Lrelr_load_loop
.Lrelr_load_done:
    ret

.Lclear_bss:
    leaq __bss_start(%rip), %rdi
    leaq __bss_end(%rip), %rcx
    subq %rdi, %rcx
    xorl %eax, %eax
    rep stosb
    ret

    .global x86_64_activate_final_address_space
    .type x86_64_activate_final_address_space, @function
x86_64_activate_final_address_space:
    cli
    movq %rdi, %r12
    movq %rsi, %r14
    movq %rdx, %r15
    leaq __image_start(%rip), %r13

    leaq __rela_dyn_start(%rip), %rsi
    leaq __rela_dyn_end(%rip), %rdi
.Lrela_virtual_loop:
    cmpq %rdi, %rsi
    jae .Lrela_virtual_done
    movq 0(%rsi), %rax
    movq 8(%rsi), %rcx
    movq 16(%rsi), %rdx
    cmpl $R_X86_64_RELATIVE, %ecx
    jne .Lhalt
    addq %r13, %rax
    addq %r14, %rdx
    movq %rdx, (%rax)
    addq $24, %rsi
    jmp .Lrela_virtual_loop
.Lrela_virtual_done:
    leaq __relr_dyn_start(%rip), %rsi
    leaq __relr_dyn_end(%rip), %rdi
    movq %r14, %r10
    subq %r13, %r10
    xorq %rbx, %rbx
.Lrelr_virtual_loop:
    cmpq %rdi, %rsi
    jae .Lrelr_virtual_done
    movq (%rsi), %rax
    addq $8, %rsi
    testq $1, %rax
    jnz .Lrelr_virtual_bitmap
    leaq (%r13,%rax), %rbx
    addq %r10, (%rbx)
    addq $8, %rbx
    jmp .Lrelr_virtual_loop
.Lrelr_virtual_bitmap:
    testq %rbx, %rbx
    jz .Lhalt
    shrq $1, %rax
    movl $63, %ecx
.Lrelr_virtual_bitmap_loop:
    testq $1, %rax
    jz .Lrelr_virtual_bitmap_next
    addq %r10, (%rbx)
.Lrelr_virtual_bitmap_next:
    addq $8, %rbx
    shrq $1, %rax
    decl %ecx
    jnz .Lrelr_virtual_bitmap_loop
    jmp .Lrelr_virtual_loop
.Lrelr_virtual_done:
    mfence
    movq %r12, %cr3
    leaq start_kernel(%rip), %rax
    subq %r13, %rax
    addq %r14, %rax
    movq %r15, %rsp
    xorq %rbp, %rbp
    jmp *%rax
    .size x86_64_activate_final_address_space, . - x86_64_activate_final_address_space
