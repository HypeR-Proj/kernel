/*
 * SPDX-FileCopyrightText: 2026 roolrz
 * SPDX-License-Identifier: Apache-2.0
 */

OUTPUT_ARCH(i386:x86-64)
ENTRY(x86_64_protected_entry)

/*
 * Allocation-free firmware discovery has large bounded scratch frames.
 * Preserve headroom for nested boot callers until that state becomes
 * caller-owned.
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

    .relr.dyn : ALIGN(8)
    {
        __relr_dyn_start = .;
        *(.relr.dyn)
        __relr_dyn_end = .;
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

    .boot_page_tables (NOLOAD) : ALIGN(4K)
    {
        __boot_pml4 = .;
        . += 4K;
        __boot_pdpt = .;
        . += 4K;
        __boot_pd = .;
        . += 4K;
    }

    .boot_stack (NOLOAD) : ALIGN(16)
    {
        __boot_stack_bottom = .;
        . += BOOT_STACK_SIZE;
        __boot_stack_top = .;
    }

    /*
     * Direct Linux boot loaders append setup_data after the protected payload
     * file. Materialize the complete NOLOAD tail in the raw image so appended
     * DTB data cannot overlap BSS, bootstrap page tables, or the boot stack.
     */
    .image_tail : ALIGN(1)
    {
        BYTE(0)
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

ASSERT(__image_start == 0, "the position-independent payload must link at zero")
ASSERT(x86_64_protected_entry == 0, "the protected-mode entry must be first")
ASSERT((__boot_pml4 & 0xFFF) == 0, "the bootstrap PML4 must be page aligned")
ASSERT((__boot_stack_top & 0xF) == 0, "the boot stack must be 16-byte aligned")
