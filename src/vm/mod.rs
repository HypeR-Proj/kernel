// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Architecture-neutral VM image formats and loading contracts.

pub mod aarch64;
pub mod arm;
pub mod device;
pub mod exit;
pub mod interrupt;
pub mod riscv64;
pub mod translation;
pub mod x86;
