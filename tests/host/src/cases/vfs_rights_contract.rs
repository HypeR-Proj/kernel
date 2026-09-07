// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use hyper::abi::native;

use crate::vfs_rights_contract::directory_rights_for_file;

#[test]
fn every_file_right_is_derived_from_the_source_directory() {
    for requested in [
        0,
        native::HYPER_NATIVE_RIGHT_READ,
        native::HYPER_NATIVE_RIGHT_EXECUTE,
        native::HYPER_NATIVE_RIGHT_DUPLICATE,
        native::HYPER_NATIVE_RIGHT_TRANSFER,
        native::HYPER_NATIVE_RIGHT_INSPECT,
        native::HYPER_NATIVE_RIGHT_READ
            | native::HYPER_NATIVE_RIGHT_EXECUTE
            | native::HYPER_NATIVE_RIGHT_TRANSFER,
    ] {
        let required = crate::require_some(directory_rights_for_file(requested));
        assert_ne!(required & native::HYPER_NATIVE_RIGHT_READ, 0);
        assert_eq!(required & requested, requested);
    }
}

#[test]
fn non_file_rights_cannot_cross_the_directory_boundary() {
    assert_eq!(
        directory_rights_for_file(native::HYPER_NATIVE_RIGHT_WRITE),
        None
    );
    assert_eq!(
        directory_rights_for_file(native::HYPER_NATIVE_RIGHT_START),
        None
    );
    assert_eq!(
        directory_rights_for_file(native::HYPER_NATIVE_RIGHT_READ | native::HYPER_NATIVE_RIGHT_MAP),
        None
    );
}
