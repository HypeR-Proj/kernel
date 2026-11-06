/* SPDX-FileCopyrightText: 2026 roolrz
 * SPDX-License-Identifier: Apache-2.0
 */
PHDRS {
    image PT_LOAD FILEHDR PHDRS FLAGS(5);
    dynamic PT_DYNAMIC FLAGS(4);
    data PT_LOAD FLAGS(4);
}
SECTIONS {
    . = SIZEOF_HEADERS;
    .hash : { *(.hash) } :image
    .dynsym : { *(.dynsym) } :image
    .dynstr : { *(.dynstr) } :image
    .dynamic : { *(.dynamic) } :image :dynamic
    .text : { *(.text .text.*) } :image
    .rodata : { *(.rodata .rodata.*) } :image
    .unexpected : { *(.got .got.* .data .data.* .bss .bss.*) } :image
    .rela.dyn : { *(.rela.*) } :image
    .rel.dyn : { *(.rel.*) } :image
    ASSERT(SIZEOF(.unexpected) == 0, "vDSO must need no relocations or writable storage")
    ASSERT(SIZEOF(.rela.dyn) == 0 && SIZEOF(.rel.dyn) == 0, "vDSO must not need relocation")
    ASSERT(. <= 0x10000, "vDSO code exceeds paired mapping extent")
    . = 0x10000;
    .vvar (NOLOAD) : { HIDDEN(__hyper_vvar = .); . += 0x1000; } :data
    /DISCARD/ : { *(.comment .eh_frame .eh_frame_hdr .note.*) }
}
