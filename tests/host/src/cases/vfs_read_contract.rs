// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use crate::vfs_read_contract::Error;

#[test]
fn bulk_read_preserves_offsets_across_batches() {
    use crate::vfs_read_contract::read_batches;
    let mut calls = 0;
    let result = read_batches::<()>(17, 2 * 1024 * 1024, 65536, |offset, done, size| {
        assert_eq!(offset, 17 + done as u64);
        assert_eq!(done, calls * 65536);
        assert_eq!(size, 65536);
        calls += 1;
        Ok(size)
    });
    assert_eq!(result, Ok(2 * 1024 * 1024));
    assert_eq!(calls, 32);
}

#[test]
fn bulk_read_returns_only_delivered_prefix_on_failure_or_short_read() {
    use crate::vfs_read_contract::{BatchError, read_batches};
    assert_eq!(
        read_batches(0, 100, 16, |_, _, _| Err("fault")),
        Err(BatchError::Transfer("fault"))
    );
    assert_eq!(
        read_batches(0, 100, 16, |_, done, size| if done == 0 {
            Ok(size)
        } else {
            Err("fault")
        }),
        Ok(16)
    );
    let mut calls = 0;
    assert_eq!(
        read_batches::<()>(0, 100, 16, |_, done, size| {
            calls += 1;
            Ok(if done == 0 { size } else { 3 })
        }),
        Ok(19)
    );
    assert_eq!(calls, 2);
    assert_eq!(
        read_batches::<()>(0, 0, 16, |_, _, _| panic!("empty read")),
        Ok(0)
    );
}

#[test]
fn bulk_read_rejects_overflow_and_invalid_backend_lengths() {
    use crate::vfs_read_contract::{BatchError, read_batches};
    assert_eq!(
        read_batches::<()>(u64::MAX, 1, 16, |_, _, _| panic!("overflow")),
        Err(BatchError::Contract(Error::ArithmeticOverflow))
    );
    assert_eq!(
        read_batches::<()>(0, 100, 16, |_, _, size| Ok(size + 1)),
        Err(BatchError::Contract(Error::InvalidBackendResult))
    );
    assert_eq!(
        read_batches::<()>(0, 100, 0, |_, _, _| panic!("zero batch")),
        Err(BatchError::Contract(Error::InvalidBackendResult))
    );
}
