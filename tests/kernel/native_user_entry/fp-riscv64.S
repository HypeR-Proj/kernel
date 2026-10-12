/*
 * SPDX-FileCopyrightText: 2026 roolrz
 * SPDX-License-Identifier: Apache-2.0
 */

/* Copied into independent U/VS mappings, never executed by the IMAC kernel. */
.section .rodata.fp_probes, "a"
.option norvc
.balign 4
.macro fp_program name, guest=0, wait=0
.global \name\()_start
.global \name\()_end
\name\()_start:
    .if \guest
    li s3, 0x4453
    mv s7, a0
    .if \wait
    /* Normal VS illegal delegation preserves the original PC and FS policy. */
    lla t0, .Lillegal_handler\@
    csrw stvec, t0
    li t0, (3 << 13)
    csrc sstatus, t0
    lla t3, .Lillegal\@
.Lillegal\@:
    .word 0xffffffff
    lla t3, .Ldisabled_fp\@
.Ldisabled_fp\@:
    fmv.x.d t0, f0
    .endif
    li t0, (1 << 13)
    csrs sstatus, t0
    .else
    lui s3, 1
    addi s3, s3, 0x235             /* The fixture patches these two words. */
    li t0, -4096
    add s7, sp, t0
    .endif
    .irp reg,f0,f1,f2,f3,f4,f5,f6,f7,f8,f9,f10,f11,f12,f13,f14,f15,f16,f17,f18,f19,f20,f21,f22,f23,f24,f25,f26,f27,f28,f29,f30,f31
    fmv.x.d t0, \reg
    bnez t0, .Lfail\@
    .endr
    frcsr t0
    bnez t0, .Lfail\@
    andi s5, s3, 31
    ori s5, s5, 64
    fscsr s5
    mv t0, s3
    .irp reg,f0,f1,f2,f3,f4,f5,f6,f7,f8,f9,f10,f11,f12,f13,f14,f15,f16,f17,f18,f19,f20,f21,f22,f23,f24,f25,f26,f27,f28,f29,f30,f31
    fmv.d.x \reg, t0
    addi t0, t0, 1
    .endr
    li s8, 0
.Lverify\@:
    mv t1, s3
    .irp reg,f0,f1,f2,f3,f4,f5,f6,f7,f8,f9,f10,f11,f12,f13,f14,f15,f16,f17,f18,f19,f20,f21,f22,f23,f24,f25,f26,f27,f28,f29,f30,f31
    fmv.x.d t0, \reg
    bne t0, t1, .Lfail\@
    addi t1, t1, 1
    .endr
    frcsr t0
    bne t0, s5, .Lfail\@
    addi s8, s8, 1
    fence rw, w
    sd s8, 0(s7)
    .if \guest
    .if \wait
    /* Sstc interrupt remains locally masked but wakes the trapped WFI. */
    rdtime t0
    li t1, 100000
    add t0, t0, t1
    csrw 0x14d, t0
    li t0, (1 << 5)
    csrs sie, t0
    wfi
    .endif
    .else
    li a7, 0
    ecall                        /* Direct ABI query. */
    li a7, -1
    ecall                        /* Deferred unknown call. */
    li a7, 6
    ecall                        /* Yield to another owner. */
    .endif
    li t0, 0x400000
.Lbusy\@:
    addi t0, t0, -1
    bnez t0, .Lbusy\@
    j .Lverify\@
    .if \guest
    .if \wait
.balign 4
.Lillegal_handler\@:
    csrr t0, scause
    li t1, 2
    bne t0, t1, .Lfail\@
    csrr t0, sepc
    bne t0, t3, .Lfail\@
    csrr t0, sstatus
    li t1, (3 << 13)
    and t0, t0, t1
    bnez t0, .Lfail\@
    addi t0, t3, 4
    csrw sepc, t0
    sret
    .endif
    .endif
.Lfail\@:
    li t0, -1
    fence rw, w
    sd t0, 0(s7)
    j .Lfail\@
\name\()_end:
.endm

fp_program riscv64_native_fp_probe
fp_program riscv64_guest_fp_probe, 1
fp_program riscv64_guest_fp_wait_probe, 1, 1
