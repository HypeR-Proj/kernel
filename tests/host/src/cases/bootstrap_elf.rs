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
fn rejects_interpreters_tls_writable_code_and_overlap() {
    for (offset, value, width) in [
        (7, 0, 1),
        (8, 1, 1),
        (16, 2, 2),
        (18, 62, 2),
        (32, u64::MAX, 8),
        (56, 17, 2),
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

#[test]
fn accepts_loader_owned_dynamic_and_relro_metadata() -> Result<(), Error> {
    let mut bytes = fixture();
    word(&mut bytes, 56, 4, 2);
    // The loader interprets these records. Kernel mappings come only from LOAD.
    for (index, kind) in [(2, 2), (3, 0x6474_e552)] {
        let ph = 64 + index * 56;
        word(&mut bytes, ph, kind, 4);
        word(&mut bytes, ph + 4, 4, 4);
        word(&mut bytes, ph + 32, 16, 8);
        word(&mut bytes, ph + 40, 16, 8);
    }
    let image = Image::parse(&bytes)?;
    assert_eq!(image.size, 8192);
    assert_eq!(image.entry, 4096);
    assert_eq!(image.segments().len(), 2);
    for unsupported in [3, 7, 0x6474_e551] {
        word(&mut bytes, 64 + 2 * 56, unsupported, 4);
        word(&mut bytes, 64 + 2 * 56 + 4, 5, 4);
        assert!(matches!(Image::parse(&bytes), Err(Error::Segment)));
    }
    Ok(())
}

#[test]
fn unaligned_load_backing_keeps_the_prefix_and_final_data_byte() -> Result<(), Error> {
    let mut bytes = fixture();
    // The second LOAD starts at the last four bytes of a page and crosses into
    // another page. Its aligned backing contains the entire preceding prefix.
    bytes.resize(12288, 0xa7);
    word(&mut bytes, 24, 8188, 8);
    let ph = 64 + 56;
    word(&mut bytes, ph + 8, 8188, 8);
    word(&mut bytes, ph + 16, 8188, 8);
    word(&mut bytes, ph + 32, 12, 8);
    word(&mut bytes, ph + 40, 20, 8);
    bytes[8188..8200].copy_from_slice(b"segment-data");
    let image = Image::parse(&bytes)?;
    let segment = &image.segments()[1];
    assert_eq!(segment.file_offset, 4096);
    assert_eq!(segment.data_offset, 4092);
    assert_eq!(segment.file_size, 12);
    assert_eq!(segment.size, 8192);
    let backing = segment.backing_bytes(&bytes)?;
    assert_eq!(backing.len(), 4104);
    assert_eq!(&backing[4092..], b"segment-data");
    assert!(segment.backing_bytes(&bytes[..8199]).is_err());
    Ok(())
}

#[test]
fn pure_bss_backing_is_empty_and_segment_extent_overflow_is_rejected() -> Result<(), Error> {
    use hyper::exec::bootstrap::Segment;
    let bss = Segment {
        file_offset: 4096,
        data_offset: 4092,
        file_size: 0,
        size: 8192,
        ..Segment::default()
    };
    assert!(bss.backing_bytes(&[])?.is_empty());
    for malformed in [
        Segment {
            file_size: 1,
            file_offset: u64::MAX,
            ..Segment::default()
        },
        Segment {
            file_size: u64::MAX,
            data_offset: 1,
            ..Segment::default()
        },
    ] {
        assert!(matches!(
            malformed.backing_bytes(&[0; 8]),
            Err(Error::Segment)
        ));
    }
    Ok(())
}
