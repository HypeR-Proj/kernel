/*
 * SPDX-FileCopyrightText: 2026 roolrz
 * SPDX-License-Identifier: Apache-2.0
 */

OUTPUT_ARCH(aarch64)
ENTRY(_start)

/*
 * Firmware discovery is allocation-free and keeps its bounded FDT walk state
 * on the bootstrap stack. Keep headroom for nested parser frames until that
 * scratch state moves into caller-owned boot storage.
 */
BOOT_STACK_SIZE = 256K;

/* The image is linked at zero. Runtime addresses must be PC-relative or
 * supplied by firmware so the loader may choose any ABI-compliant address. */
SECTIONS
{
    . = 0;
    __image_start = .;

    .text : ALIGN(4K)
    {
        __text_start = .;
        KEEP(*(.text.boot.header))
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

    .relr.dyn : ALIGN(8)
    {
        __relr_dyn_start = .;
        *(.relr.dyn)
        __relr_dyn_end = .;
    }

    .dynamic : ALIGN(8)
    {
        *(.dynamic)
    }

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

    /* Built at runtime because descriptors contain physical addresses. */
    .boot_page_tables (NOLOAD) : ALIGN(4K)
    {
        __boot_l0_table = .;
        . += 4K;
        __boot_l1_table = .;
        . += 4K;
        __boot_l1_table_end = .;
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
ASSERT((_start - __image_start) == 0, "the Linux Image header must be first")
ASSERT((__boot_l0_table & 0xFFF) == 0, "boot L0 table must be page aligned")
ASSERT((__boot_l1_table & 0xFFF) == 0, "boot L1 table must be page aligned")
ASSERT((__boot_stack_top & 0xF) == 0, "boot stack must be 16-byte aligned")
