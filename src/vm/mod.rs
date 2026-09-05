// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Architecture-neutral VM image formats and loading contracts.

pub mod aarch64;
pub mod arm;
pub mod bundle;
pub mod exit;
pub mod fdt;
pub mod interrupt;
pub mod translation;
pub mod x86;
