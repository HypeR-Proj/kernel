// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Bounded scalar-input policy shared by `ProcessBuilder` entry paths.

const _: () = assert!(hyper::abi::native::HYPER_NATIVE_PROCESS_NAME_MAX_BYTES <= usize::MAX as u64);
pub(crate) const MAX_NAME_BYTES: usize =
    hyper::abi::native::HYPER_NATIVE_PROCESS_NAME_MAX_BYTES as usize;
pub(crate) const MAX_STARTUP_DATA_BYTES: usize =
    hyper::abi::native::HYPER_NATIVE_PROCESS_STARTUP_DATA_MAX_BYTES as usize;
pub(crate) const MAX_STARTUP_HANDLES: usize = 256;
pub(crate) const ABI_AFFINITY_WORDS: usize = 4;

pub(crate) fn valid_name(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_NAME_BYTES && !value.as_bytes().contains(&0)
}
