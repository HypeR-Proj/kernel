// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

#[cfg(all(target_arch = "aarch64", not(CONFIG_ARCH_AARCH64)))]
compile_error!("the AArch64 target requires CONFIG_ARCH_AARCH64=y");

#[cfg(target_arch = "aarch64")]
pub(crate) mod aarch64;
#[cfg(target_arch = "riscv64")]
pub(crate) mod riscv64;
#[cfg(target_arch = "x86_64")]
pub(crate) mod x86_64;

#[cfg(all(target_arch = "riscv64", not(CONFIG_ARCH_RISCV64)))]
compile_error!("the RISC-V target requires CONFIG_ARCH_RISCV64=y");

#[cfg(all(target_arch = "x86_64", not(CONFIG_ARCH_X86_64)))]
compile_error!("the x86-64 target requires CONFIG_ARCH_X86_64=y");

#[cfg(target_arch = "aarch64")]
use aarch64 as imp;
#[cfg(target_arch = "riscv64")]
use riscv64 as imp;
#[cfg(target_arch = "x86_64")]
use x86_64 as imp;

pub(crate) mod context;
pub(crate) mod cpu;
pub(crate) mod exception;
pub(crate) mod irq;
pub(crate) mod memory;
pub(crate) mod platform;
pub(crate) mod time;
#[cfg(any(target_arch = "aarch64", target_arch = "riscv64"))]
pub(crate) mod user;
pub(crate) mod vm;
