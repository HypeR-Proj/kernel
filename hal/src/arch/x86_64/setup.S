/*
 * SPDX-FileCopyrightText: 2026 roolrz
 * SPDX-License-Identifier: Apache-2.0
 */

    .section .setup, "ax"
    .code16
    .org 0x1f1
    .byte 4                       /* setup_sects: five sectors total */
    .org 0x1fa
    .word 0xffff                  /* vid_mode */
    .org 0x1fe
    .word 0xaa55
    .org 0x200
    jmp .Lsetup_entry
    .org 0x202
    .ascii "HdrS"
    .org 0x206
    .word 0x020f                  /* Linux boot protocol 2.15 */
    .org 0x210
    .byte 0xff                    /* type_of_loader, patched by QEMU */
    .org 0x211
    .byte 0x01                    /* LOADED_HIGH */
    .org 0x214
    .long 0x00100000              /* code32_start, patched by QEMU */
    .org 0x218
    .long 0
    .org 0x21c
    .long 0
    .org 0x224
    .word 0
    .org 0x228
    .long 0
    .org 0x22c
    .long 0x37ffffff
    .org 0x230
    .long 0x00200000              /* kernel_alignment */
    .org 0x234
    .byte 1                       /* relocatable_kernel */
    .org 0x235
    .byte 12                      /* min_alignment */
    .org 0x236
    .word 1                       /* XLF_KERNEL_64 */
    .org 0x238
    .long 2048                    /* cmdline_size */
    .org 0x250
    .quad 0                       /* setup_data, patched by QEMU */
    .org 0x258
    .quad 0x00200000              /* pref_address */
    .org 0x260
    .long 0x02000000              /* init_size */
    .org 0x264
    .long 0

    .org 0x280
.Lsetup_entry:
    cli
    cld

    /* QEMU enters the setup payload at physical real_addr + 0x200. */
    xorl %eax, %eax
    movw %cs, %ax
    shll $4, %eax
    movl %eax, %esi
    subl $0x200, %esi

    movw %cs, %ax
    movw %ax, %ds
    movw %ax, %es
    movw %ax, %ss
    movw $0x7f0, %sp

    xorl %eax, %eax
    movw %cs, %ax
    shll $4, %eax
    addl $(.Lgdt - 0x200), %eax
    movl %eax, (.Lgdt_descriptor + 2 - 0x200)
    lgdtl (.Lgdt_descriptor - 0x200)

    movl %cs:0x14, %eax
    movl %eax, (.Lprotected_pointer - 0x200)
    movw $0x10, (.Lprotected_pointer + 4 - 0x200)

    movl %cr0, %eax
    orl $1, %eax
    movl %eax, %cr0
    ljmpl *(.Lprotected_pointer - 0x200)

    .balign 8
.Lgdt:
    .quad 0
    .quad 0
    .quad 0x00cf9a000000ffff      /* 0x10: 32-bit code */
    .quad 0x00cf92000000ffff      /* 0x18: data */
    .quad 0x00af9a000000ffff      /* 0x20: 64-bit code */
.Lgdt_end:
.Lgdt_descriptor:
    .word .Lgdt_end - .Lgdt - 1
    .long 0
.Lprotected_pointer:
    .long 0
    .word 0

    .org 0xa00
