// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Kernel-owned application binary interface policy.
//!
//! Public fixed-width declarations live in [`hyper::abi`]. This module owns
//! validation, capability operations, user-copy policy, and translation from
//! kernel errors to public status values.
pub(crate) mod native;
