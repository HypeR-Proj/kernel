// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Architecture-neutral VM image formats and loading contracts.

pub mod aarch64;
pub mod arm;
#[cfg(any(feature = "host-vm-model-tests", feature = "kernel-self-test"))]
pub mod bundle;
pub mod exit;
#[cfg(feature = "kernel-self-test")]
pub mod fdt;
pub mod interrupt;
pub mod translation;
pub mod x86;
