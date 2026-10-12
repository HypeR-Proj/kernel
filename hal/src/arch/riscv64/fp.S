/*
 * SPDX-FileCopyrightText: 2026 roolrz
 * SPDX-License-Identifier: Apache-2.0
 */

#include "asm_constants.h"
.section .text.fp, "ax"
.balign 4

.macro fp_enable
    li t0, SSTATUS_FS_MASK
    csrs sstatus, t0
.endm
.macro fp_disable
    li t0, SSTATUS_FS_MASK
    csrc sstatus, t0
.endm
.macro fp_clear
    fmv.d.x f0, zero
    fmv.d.x f1, zero
    fmv.d.x f2, zero
    fmv.d.x f3, zero
    fmv.d.x f4, zero
    fmv.d.x f5, zero
    fmv.d.x f6, zero
    fmv.d.x f7, zero
    fmv.d.x f8, zero
    fmv.d.x f9, zero
    fmv.d.x f10, zero
    fmv.d.x f11, zero
    fmv.d.x f12, zero
    fmv.d.x f13, zero
    fmv.d.x f14, zero
    fmv.d.x f15, zero
    fmv.d.x f16, zero
    fmv.d.x f17, zero
    fmv.d.x f18, zero
    fmv.d.x f19, zero
    fmv.d.x f20, zero
    fmv.d.x f21, zero
    fmv.d.x f22, zero
    fmv.d.x f23, zero
    fmv.d.x f24, zero
    fmv.d.x f25, zero
    fmv.d.x f26, zero
    fmv.d.x f27, zero
    fmv.d.x f28, zero
    fmv.d.x f29, zero
    fmv.d.x f30, zero
    fmv.d.x f31, zero
    fscsr zero
.endm

.global riscv64_fp_reset
.type riscv64_fp_reset, @function
riscv64_fp_reset:
    fp_enable
    fp_clear
    fp_disable
    ret
.size riscv64_fp_reset, . - riscv64_fp_reset

.global riscv64_fp_restore
.type riscv64_fp_restore, @function
riscv64_fp_restore:
    fp_enable
    fld f0, 0(a0)
    fld f1, 8(a0)
    fld f2, 16(a0)
    fld f3, 24(a0)
    fld f4, 32(a0)
    fld f5, 40(a0)
    fld f6, 48(a0)
    fld f7, 56(a0)
    fld f8, 64(a0)
    fld f9, 72(a0)
    fld f10, 80(a0)
    fld f11, 88(a0)
    fld f12, 96(a0)
    fld f13, 104(a0)
    fld f14, 112(a0)
    fld f15, 120(a0)
    fld f16, 128(a0)
    fld f17, 136(a0)
    fld f18, 144(a0)
    fld f19, 152(a0)
    fld f20, 160(a0)
    fld f21, 168(a0)
    fld f22, 176(a0)
    fld f23, 184(a0)
    fld f24, 192(a0)
    fld f25, 200(a0)
    fld f26, 208(a0)
    fld f27, 216(a0)
    fld f28, 224(a0)
    fld f29, 232(a0)
    fld f30, 240(a0)
    fld f31, 248(a0)
    lw t0, 256(a0)
    fscsr t0
    fp_disable
    ret
.size riscv64_fp_restore, . - riscv64_fp_restore

.global riscv64_fp_save_and_clear
.type riscv64_fp_save_and_clear, @function
riscv64_fp_save_and_clear:
    fp_enable
    fsd f0, 0(a0)
    fsd f1, 8(a0)
    fsd f2, 16(a0)
    fsd f3, 24(a0)
    fsd f4, 32(a0)
    fsd f5, 40(a0)
    fsd f6, 48(a0)
    fsd f7, 56(a0)
    fsd f8, 64(a0)
    fsd f9, 72(a0)
    fsd f10, 80(a0)
    fsd f11, 88(a0)
    fsd f12, 96(a0)
    fsd f13, 104(a0)
    fsd f14, 112(a0)
    fsd f15, 120(a0)
    fsd f16, 128(a0)
    fsd f17, 136(a0)
    fsd f18, 144(a0)
    fsd f19, 152(a0)
    fsd f20, 160(a0)
    fsd f21, 168(a0)
    fsd f22, 176(a0)
    fsd f23, 184(a0)
    fsd f24, 192(a0)
    fsd f25, 200(a0)
    fsd f26, 208(a0)
    fsd f27, 216(a0)
    fsd f28, 224(a0)
    fsd f29, 232(a0)
    fsd f30, 240(a0)
    fsd f31, 248(a0)
    frcsr t0
    sw t0, 256(a0)
    fp_clear
    fp_disable
    ret
.size riscv64_fp_save_and_clear, . - riscv64_fp_save_and_clear
