/*
 * SPDX-FileCopyrightText: 2026 roolrz
 * SPDX-License-Identifier: Apache-2.0
 */

OUTPUT_ARCH(riscv)
ENTRY(_start)

/*
 * Allocation-free FDT discovery has large bounded scratch frames. Preserve
 * headroom for nested boot callers until that state becomes caller-owned.
 */
BOOT_STACK_SIZE = 256K;

SECTIONS
{
    . = 0;
    __image_start = .;

    .text : ALIGN(4K)
    {
        __text_start = .;
        KEEP(*(.text.boot))
        *(.text .text.*)
        . = ALIGN(4K);
        __text_end = .;
    }

    .rodata : ALIGN(4K)
    {
        __rodata_start = .;
        *(.rodata .rodata.*)
    }

    .rela.dyn : ALIGN(8)
    {
        __rela_dyn_start = .;
        *(.rela.dyn)
        __rela_dyn_end = .;
    }
    .dynamic : ALIGN(8) { *(.dynamic) }
    .dynsym : ALIGN(8)
    {
        __kallsyms_symbols_start = .;
        *(.dynsym)
        __kallsyms_symbols_end = .;
    }
    .dynstr : ALIGN(1)
    {
        __kallsyms_strings_start = .;
        *(.dynstr)
        __kallsyms_strings_end = .;
    }
    .kallsyms : ALIGN(8)
    {
        __kallsyms_start = .;
        KEEP(*(.kallsyms))
        __kallsyms_end = .;
    }
    .gnu.hash : ALIGN(8) { *(.gnu.hash) }
    .hash : ALIGN(8) { *(.hash) }

    . = ALIGN(4K);
    __rodata_end = .;

    .data : ALIGN(4K)
    {
        __data_start = .;
        *(.data .data.*)
        *(.got .got.*)
    }

    .bss (NOLOAD) : ALIGN(16)
    {
        __bss_start = .;
        *(.bss .bss.*)
        *(COMMON)
        . = ALIGN(8);
        __bss_end = .;
    }

    .boot_stack (NOLOAD) : ALIGN(16)
    {
        __boot_stack_bottom = .;
        . += BOOT_STACK_SIZE;
        __boot_stack_top = .;
    }

    __image_end = .;
    __image_size = __image_end - __image_start;

    /DISCARD/ :
    {
        *(.comment)
        *(.eh_frame .eh_frame.*)
        *(.eh_frame_hdr)
        *(.note .note.*)
    }
}

ASSERT(__image_start == 0, "the position-independent image must link at zero")
ASSERT((_start - __image_start) == 0, "the RISC-V entry must be first")
ASSERT((__boot_stack_top & 0xF) == 0, "boot stack must be 16-byte aligned")
