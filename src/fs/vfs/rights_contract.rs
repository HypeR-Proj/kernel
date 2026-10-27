// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! ABI-shaped capability-derivation contract for VFS objects.

use hyper::abi::native;

pub(crate) const FILE_SUPPORTED_RIGHTS: u64 = native::HYPER_NATIVE_RIGHT_DUPLICATE
    | native::HYPER_NATIVE_RIGHT_TRANSFER
    | native::HYPER_NATIVE_RIGHT_INSPECT
    | native::HYPER_NATIVE_RIGHT_WRITE
    | native::HYPER_NATIVE_RIGHT_READ
    | native::HYPER_NATIVE_RIGHT_EXECUTE
    | native::HYPER_NATIVE_RIGHT_SET_ATTRIBUTES
    | native::HYPER_NATIVE_RIGHT_LOCK_FILE
    | native::HYPER_NATIVE_RIGHT_PROTECT;

pub(crate) const DIRECTORY_SUPPORTED_RIGHTS: u64 = native::HYPER_NATIVE_RIGHT_DUPLICATE
    | native::HYPER_NATIVE_RIGHT_TRANSFER
    | native::HYPER_NATIVE_RIGHT_INSPECT
    | native::HYPER_NATIVE_RIGHT_WRITE
    | native::HYPER_NATIVE_RIGHT_READ
    | native::HYPER_NATIVE_RIGHT_EXECUTE
    | native::HYPER_NATIVE_RIGHT_SET_ATTRIBUTES
    | native::HYPER_NATIVE_RIGHT_LOCK_FILE
    | native::HYPER_NATIVE_RIGHT_PROTECT;

pub(crate) const DIRECTORY_STREAM_SUPPORTED_RIGHTS: u64 = native::HYPER_NATIVE_RIGHT_READ
    | native::HYPER_NATIVE_RIGHT_DUPLICATE
    | native::HYPER_NATIVE_RIGHT_TRANSFER
    | native::HYPER_NATIVE_RIGHT_INSPECT;

/// Enumeration preserves restrictions on inspection and capability propagation.
/// The caller separately verifies READ before deriving this grant.
pub(crate) const fn directory_stream_rights(source: u64) -> u64 {
    source & DIRECTORY_STREAM_SUPPORTED_RIGHTS
}

/// Computes the source authority required to create one File capability.
///
/// Traversal always requires `READ`. Every right carried by the result must
/// already be present on the source Directory, preserving monotonic authority
/// across the cross-object derivation.
pub(crate) const fn directory_rights_for_file(requested: u64) -> Option<u64> {
    if requested & !FILE_SUPPORTED_RIGHTS == 0 {
        Some(native::HYPER_NATIVE_RIGHT_READ | requested)
    } else {
        None
    }
}

pub(crate) const fn directory_rights_for_directory(requested: u64) -> Option<u64> {
    if requested & !DIRECTORY_SUPPORTED_RIGHTS == 0 {
        Some(native::HYPER_NATIVE_RIGHT_READ | requested)
    } else {
        None
    }
}
