// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use hyper::hal::cache::{PublicationLayout, PublicationLayoutError, page_ownership_supports_line};
use hyper::mm::{PhysicalAddress, VirtualAddress};

fn pa(value: u64) -> PhysicalAddress {
    PhysicalAddress::new(value)
}

fn va(value: u64) -> VirtualAddress {
    VirtualAddress::new(value)
}

#[test]
fn page_ownership_admits_only_dividing_cache_lines() {
    assert!(page_ownership_supports_line(64, 4096));
    assert!(page_ownership_supports_line(4096, 4096));

    assert!(!page_ownership_supports_line(0, 4096));
    assert!(!page_ownership_supports_line(64, 0));
    assert!(!page_ownership_supports_line(96, 4096));
    assert!(!page_ownership_supports_line(64, 6144));
    assert!(!page_ownership_supports_line(8192, 4096));
}

#[test]
fn publication_layout_aligns_the_firmware_address_before_its_linear_alias() {
    let layout = super::require_ok(PublicationLayout::new(
        pa(0x1003),
        va(0xffff_0000_0000_1003),
        4096,
        72,
        8,
        64,
    ));

    assert_eq!(layout.physical_address(), pa(0x1040));
    assert_eq!(layout.virtual_address(), va(0xffff_0000_0000_1040));
    assert_eq!(layout.published_size(), 128);
}

#[test]
fn publication_layout_rejects_an_incongruent_virtual_alias() {
    assert_eq!(
        PublicationLayout::new(pa(0x1003), va(0xffff_0000_0000_1000), 4096, 72, 8, 64),
        Err(PublicationLayoutError::VirtualAliasMisaligned)
    );
}

#[test]
fn publication_layout_honors_typed_alignment_larger_than_a_cache_line() {
    let layout = super::require_ok(PublicationLayout::new(
        pa(0x1003),
        va(0xffff_0000_0000_1003),
        512,
        32,
        256,
        64,
    ));
    assert_eq!(layout.physical_address(), pa(0x1100));
    assert_eq!(layout.virtual_address(), va(0xffff_0000_0000_1100));
    assert_eq!(layout.published_size(), 64);

    assert_eq!(
        PublicationLayout::new(pa(0x1003), va(0xffff_0000_0000_1004), 512, 32, 256, 64),
        Err(PublicationLayoutError::VirtualAliasMisaligned)
    );
}

#[test]
fn publication_layout_contains_the_complete_rounded_cache_range() {
    let exact = super::require_ok(PublicationLayout::new(
        pa(0x103f),
        va(0xffff_0000_0000_103f),
        129,
        65,
        8,
        64,
    ));
    assert_eq!(exact.physical_address(), pa(0x1040));
    assert_eq!(exact.published_size(), 128);

    assert_eq!(
        PublicationLayout::new(pa(0x103f), va(0xffff_0000_0000_103f), 128, 65, 8, 64),
        Err(PublicationLayoutError::OutsideOwnedRange)
    );
}

#[test]
fn publication_layout_rejects_invalid_geometry_and_overflow() {
    assert_eq!(
        PublicationLayout::new(pa(0), va(0), 4096, 0, 8, 64),
        Err(PublicationLayoutError::EmptyPayload)
    );
    assert_eq!(
        PublicationLayout::new(pa(0), va(0), 4096, 8, 3, 64),
        Err(PublicationLayoutError::InvalidAlignment)
    );
    assert_eq!(
        PublicationLayout::new(pa(0), va(0), 4096, 8, 8, 0),
        Err(PublicationLayoutError::InvalidAlignment)
    );
    assert_eq!(
        PublicationLayout::new(pa(0), va(0), 4096, 8, 8, 96),
        Err(PublicationLayoutError::InvalidAlignment)
    );
    assert_eq!(
        PublicationLayout::new(pa(u64::MAX), va(usize::MAX as u64), 4096, 8, 8, 64),
        Err(PublicationLayoutError::AddressOverflow)
    );
    assert_eq!(
        PublicationLayout::new(pa(u64::MAX - 63), va(0x1000), 64, 64, 8, 64),
        Err(PublicationLayoutError::AddressOverflow)
    );
    assert_eq!(
        PublicationLayout::new(pa(0x1000), va((usize::MAX - 63) as u64), 64, 64, 8, 64),
        Err(PublicationLayoutError::AddressOverflow)
    );
    assert_eq!(
        PublicationLayout::new(pa(u64::MAX - 127), va(0x1000), 4096, 64, 8, 64),
        Err(PublicationLayoutError::AddressOverflow)
    );
    assert_eq!(
        PublicationLayout::new(pa(0x1000), va((usize::MAX - 127) as u64), 4096, 64, 8, 64),
        Err(PublicationLayoutError::AddressOverflow)
    );
    assert_eq!(
        PublicationLayout::new(pa(0), va(0), usize::MAX, usize::MAX, 8, 64),
        Err(PublicationLayoutError::AddressOverflow)
    );
}
