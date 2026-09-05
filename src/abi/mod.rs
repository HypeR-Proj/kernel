// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Architecture-neutral contracts shared by userspace and kernel adapters.
//!
//! This module owns machine-visible values and owned entry payloads. Syscall
//! dispatch, capability validation, and architecture register encoding remain
//! in their respective kernel and architecture modules.

pub mod native;
