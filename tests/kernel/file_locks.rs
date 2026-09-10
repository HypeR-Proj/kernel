// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Runtime file-lock tests retain worker context through scheduler quiescence.

pub(super) fn run() -> Result<(), impl core::fmt::Debug> {
    crate::kernel::vfs::locks::run_self_test(|| super::support::quiesce_workers().is_ok())
}
