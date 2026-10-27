// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Allocation-free runtime model for an Arm Generic Interrupt Controller.

mod controller;
pub mod lr;
pub mod lr_v2;
pub mod mmio;
pub mod quiesce;
mod ready;

pub use controller::{
    BuildError, GicInterruptId, InterruptGroup, InterruptSnapshot, InterruptTrigger, ListEntry,
    ListState, RuntimeError, VirtualGic, VirtualGicBuilder,
};
