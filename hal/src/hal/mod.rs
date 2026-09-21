// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Statically selected machine capabilities for the kernel binary.
//!
//! Reusable architecture-neutral contracts live in [`hyper::hal`]. This
//! crate-private adapter binds those contracts to the selected architecture and
//! is the sole machine-operation dependency exposed to kernel policy.

pub mod atomic;
pub mod cache;
pub mod context;
pub mod cpu;
pub mod exception;
pub mod irq;
pub mod memory;
pub mod platform;
pub mod time;
pub mod user;
pub mod vm;
