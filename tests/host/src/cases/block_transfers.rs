// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use crate::block_transfer::decode_write_batch;
use hyper::fs::block::Error;

fn frame() -> Vec<u8> {
    let mut bytes = Vec::new();
    for first in [4_u64, 12] {
        bytes.extend_from_slice(&first.to_le_bytes());
        bytes.extend_from_slice(&512_u64.to_le_bytes());
    }
    bytes.extend_from_slice(&[0x13; 512]);
    bytes.extend_from_slice(&[0x27; 512]);
    bytes
}
#[test]
fn disjoint_batch_borrows_complete_payloads_in_order() {
    let bytes = frame();
    let batch = crate::require_ok(decode_write_batch(&bytes, 2, 100));
    let requests = batch.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].first, 4);
    assert_eq!(requests[0].bytes, [0x13; 512]);
    assert_eq!(requests[1].first, 12);
    assert_eq!(requests[1].bytes, [0x27; 512]);
}
#[test]
fn invalid_batch_never_yields_a_partial_submission() {
    let bytes = frame();
    for count in [0, 5, usize::MAX] {
        assert!(matches!(
            decode_write_batch(&bytes, count, 100),
            Err(Error::InvalidRange)
        ));
    }
    assert!(decode_write_batch(&bytes[..31], 2, 100).is_err());
    assert!(decode_write_batch(&bytes[..bytes.len() - 1], 2, 100).is_err());
    let mut extra = bytes.clone();
    extra.push(0);
    assert!(decode_write_batch(&extra, 2, 100).is_err());
    for (offset, value) in [(8, u64::MAX), (0, u64::MAX), (16, 4), (8, 511)] {
        let mut invalid = bytes.clone();
        invalid[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
        assert!(decode_write_batch(&invalid, 2, u64::MAX).is_err());
    }
    assert!(decode_write_batch(&bytes, 2, 12).is_err());
    let oversized = vec![0; 512 * 1024 + 33];
    assert!(decode_write_batch(&oversized, 2, 100).is_err());
}
