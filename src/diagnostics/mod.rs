// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Kernel observability and failure handling.
pub mod crash;
pub mod debug;
pub(crate) mod inspect;
pub mod log;
pub(crate) mod profile;
pub mod trace;
