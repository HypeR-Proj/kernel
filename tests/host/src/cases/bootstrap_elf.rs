// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! The kernel admits only the bootstrap ELF subset, not app dynamic policy.

use hyper::exec::bootstrap::{Error, Image, Machine};

fn word(bytes: &mut [u8], offset: usize, value: u64, width: usize) {
    bytes[offset..offset + width].copy_from_slice(&value.to_le_bytes()[..width]);
}

fn fixture() -> Vec<u8> {
    let mut bytes = vec![0; 8192];
    bytes[..8].copy_from_slice(b"\x7fELF\x02\x01\x01\x3f");
    for (offset, value, width) in [
        (16, 3, 2),
        (18, 183, 2),
        (20, 1, 4),
        (24, 4096, 8),
        (32, 64, 8),
        (52, 64, 2),
        (54, 56, 2),
        (56, 2, 2),
    ] {
        word(&mut bytes, offset, value, width);
    }
    for i in 0..2 {
        let ph = 64 + i * 56;
        for (offset, value, width) in [
            (0, 1, 4),
            (4, 4 + i as u64, 4),
            (8, i as u64 * 4096, 8),
            (16, i as u64 * 4096, 8),
            (32, 4096, 8),
            (40, 4096, 8),
            (48, 4096, 8),
        ] {
            word(&mut bytes, ph + offset, value, width);
        }
    }
    bytes
}

#[test]
fn admits_only_bounded_native_bootstrap_segments() -> Result<(), Error> {
    let bytes = fixture();
    let image = Image::parse(&bytes)?;
    assert_eq!(image.machine, Machine::Aarch64);
    assert_eq!(image.entry, 4096);
    assert_eq!(image.size, 8192);
    assert_eq!(image.segments().len(), 2);
    assert!(image.segments()[1].executable);
    assert!(!image.segments()[1].writable);
    let mut riscv = bytes;
    word(&mut riscv, 18, 243, 2);
    word(&mut riscv, 48, 5, 4);
    word(&mut riscv, 56, 3, 2);
    word(&mut riscv, 64 + 2 * 56, 0x7000_0003, 4); // PT_RISCV_ATTRIBUTES
    assert_eq!(Image::parse(&riscv)?.machine, Machine::Riscv64);
    Ok(())
}

#[test]
fn rejects_dynamic_policy_writable_code_and_overlap() {
    for (offset, value, width) in [
        (7, 0, 1),
        (8, 1, 1),
        (16, 2, 2),
        (18, 62, 2),
        (32, u64::MAX, 8),
        (56, 17, 2),
        (64, 2, 4),
        (64, 3, 4),
        (64, 7, 4),
        (124, 7, 4),
        (136, 0, 8),
        (128, u64::MAX, 8),
        (152, 8192, 8),
        (160, u64::MAX, 8),
        (168, 65536, 8),
        (24, 0, 8),
    ] {
        let mut bytes = fixture();
        word(&mut bytes, offset, value, width);
        assert!(
            Image::parse(&bytes).is_err(),
            "accepted field {offset}={value}"
        );
    }
}

#[test]
fn truncated_headers_and_segments_never_read_past_input() {
    let bytes = fixture();
    for length in 0..bytes.len() {
        assert!(Image::parse(&bytes[..length]).is_err());
    }
}

#[test]
fn bootstrap_mapping_can_be_retired_as_one_range() {
    let mut bytes = fixture();
    word(&mut bytes, 24, 8192, 8);
    word(&mut bytes, 64 + 56 + 16, 8192, 8);
    assert!(matches!(Image::parse(&bytes), Err(Error::Segment)));
}
