// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! AMD SVM MSR, VMCB, intercept, and exit-code definitions.

#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
pub(crate) const MSR_VM_CR: u32 = 0xc001_0114;
pub(crate) const MSR_VM_HSAVE_PA: u32 = 0xc001_0117;
pub(crate) const EFER_SVME: u64 = 1 << 12;
#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
pub(crate) const VM_CR_SVM_DISABLE: u64 = 1 << 4;

pub(crate) const VMCB_INTERCEPT_WORD3: usize = 0x0c;
pub(crate) const VMCB_INTERCEPT_WORD4: usize = 0x10;
pub(crate) const VMCB_IOPM_BASE: usize = 0x40;
pub(crate) const VMCB_MSRPM_BASE: usize = 0x48;
pub(crate) const VMCB_ASID: usize = 0x58;
pub(crate) const VMCB_TLB_CONTROL: usize = 0x5c;
pub(crate) const VMCB_INT_CONTROL: usize = 0x60;
pub(crate) const VMCB_INT_VECTOR: usize = 0x64;
pub(crate) const VMCB_EXIT_CODE: usize = 0x70;
pub(crate) const VMCB_EXIT_INFO1: usize = 0x78;
pub(crate) const VMCB_EXIT_INFO2: usize = 0x80;
pub(crate) const VMCB_NESTED_CONTROL: usize = 0x90;
pub(crate) const VMCB_EVENT_INJECTION: usize = 0xa8;
pub(crate) const VMCB_EVENT_ERROR: usize = 0xac;
pub(crate) const VMCB_NESTED_CR3: usize = 0xb0;
pub(crate) const VMCB_CLEAN_BITS: usize = 0xc0;
pub(crate) const VMCB_NEXT_RIP: usize = 0xc8;

pub(crate) const SAVE_ES: usize = 0x400;
pub(crate) const SAVE_CS: usize = 0x410;
pub(crate) const SAVE_SS: usize = 0x420;
pub(crate) const SAVE_DS: usize = 0x430;
pub(crate) const SAVE_FS: usize = 0x440;
pub(crate) const SAVE_GS: usize = 0x450;
pub(crate) const SAVE_GDTR: usize = 0x460;
pub(crate) const SAVE_LDTR: usize = 0x470;
pub(crate) const SAVE_IDTR: usize = 0x480;
pub(crate) const SAVE_TR: usize = 0x490;
pub(crate) const SAVE_CPL: usize = 0x4cb;
pub(crate) const SAVE_EFER: usize = 0x4d0;
pub(crate) const SAVE_CR4: usize = 0x548;
pub(crate) const SAVE_CR3: usize = 0x550;
pub(crate) const SAVE_CR0: usize = 0x558;
pub(crate) const SAVE_DR7: usize = 0x560;
pub(crate) const SAVE_DR6: usize = 0x568;
pub(crate) const SAVE_RFLAGS: usize = 0x570;
pub(crate) const SAVE_RIP: usize = 0x578;
pub(crate) const SAVE_RSP: usize = 0x5d8;
pub(crate) const SAVE_RAX: usize = 0x5f8;
pub(crate) const SAVE_STAR: usize = 0x600;
pub(crate) const SAVE_LSTAR: usize = 0x608;
pub(crate) const SAVE_CSTAR: usize = 0x610;
pub(crate) const SAVE_SFMASK: usize = 0x618;
pub(crate) const SAVE_KERNEL_GS_BASE: usize = 0x620;
pub(crate) const SAVE_SYSENTER_CS: usize = 0x628;
pub(crate) const SAVE_SYSENTER_ESP: usize = 0x630;
pub(crate) const SAVE_SYSENTER_EIP: usize = 0x638;
pub(crate) const SAVE_PAT: usize = 0x668;

pub(crate) const INTERCEPT_INTR: u32 = 1;
pub(crate) const INTERCEPT_CPUID: u32 = 1 << 18;
pub(crate) const INTERCEPT_HLT: u32 = 1 << 24;
pub(crate) const INTERCEPT_IO: u32 = 1 << 27;
pub(crate) const INTERCEPT_MSR: u32 = 1 << 28;
pub(crate) const INTERCEPT_SHUTDOWN: u32 = 1 << 31;
pub(crate) const INTERCEPT_SVM_INSTRUCTIONS: u32 = 0x7f;

pub(crate) const EXIT_INTR: u64 = 0x60;
pub(crate) const EXIT_CPUID: u64 = 0x72;
pub(crate) const EXIT_HLT: u64 = 0x78;
pub(crate) const EXIT_IO: u64 = 0x7b;
pub(crate) const EXIT_MSR: u64 = 0x7c;
pub(crate) const EXIT_SHUTDOWN: u64 = 0x7f;
pub(crate) const EXIT_NPF: u64 = 0x400;

pub(crate) const V_IRQ: u32 = 1 << 8;
pub(crate) const V_INTR_PRIORITY_SHIFT: u32 = 16;
pub(crate) const V_IGNORE_TPR: u32 = 1 << 20;
pub(crate) const V_INTR_MASKING: u32 = 1 << 24;
pub(crate) const EVENT_VALID: u32 = 1 << 31;
pub(crate) const EVENT_ERROR_VALID: u32 = 1 << 11;
pub(crate) const EVENT_EXCEPTION: u32 = 3 << 8;
pub(crate) const EXCEPTION_GENERAL_PROTECTION: u32 = 13;
