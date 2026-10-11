// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use hyper::abi::native;

use crate::vfs_rights_contract::{
    directory_rights_for_directory, directory_rights_for_file, directory_stream_rights,
};

#[test]
fn directory_stream_cannot_restore_attenuated_rights() {
    let read = native::HYPER_NATIVE_RIGHT_READ;
    let propagation = [
        native::HYPER_NATIVE_RIGHT_DUPLICATE,
        native::HYPER_NATIVE_RIGHT_TRANSFER,
        native::HYPER_NATIVE_RIGHT_INSPECT,
    ];
    for combination in 0_u8..8 {
        let mut source = read;
        for (index, right) in propagation.into_iter().enumerate() {
            if combination & (1 << index) != 0 {
                source |= right;
            }
        }
        assert_eq!(directory_stream_rights(source), source);
        assert_eq!(
            directory_stream_rights(source | native::HYPER_NATIVE_RIGHT_WRITE),
            source
        );
    }
}

#[test]
fn every_file_right_is_derived_from_the_source_directory() {
    for requested in [
        0,
        native::HYPER_NATIVE_RIGHT_READ,
        native::HYPER_NATIVE_RIGHT_WRITE,
        native::HYPER_NATIVE_RIGHT_EXECUTE,
        native::HYPER_NATIVE_RIGHT_DUPLICATE,
        native::HYPER_NATIVE_RIGHT_TRANSFER,
        native::HYPER_NATIVE_RIGHT_INSPECT,
        native::HYPER_NATIVE_RIGHT_PROTECT,
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
        directory_rights_for_file(native::HYPER_NATIVE_RIGHT_START),
        None
    );
    assert_eq!(
        directory_rights_for_file(native::HYPER_NATIVE_RIGHT_READ | native::HYPER_NATIVE_RIGHT_MAP),
        None
    );
}

#[test]
fn child_directory_rights_are_monotonically_derived() {
    for requested in [
        0,
        native::HYPER_NATIVE_RIGHT_READ,
        native::HYPER_NATIVE_RIGHT_WRITE,
        native::HYPER_NATIVE_RIGHT_EXECUTE,
        native::HYPER_NATIVE_RIGHT_DUPLICATE,
        native::HYPER_NATIVE_RIGHT_TRANSFER,
        native::HYPER_NATIVE_RIGHT_INSPECT,
        native::HYPER_NATIVE_RIGHT_PROTECT,
    ] {
        let required = crate::require_some(directory_rights_for_directory(requested));
        assert_ne!(required & native::HYPER_NATIVE_RIGHT_READ, 0);
        assert_eq!(required & requested, requested);
    }

    assert_eq!(
        directory_rights_for_directory(native::HYPER_NATIVE_RIGHT_MAP),
        None
    );
}
