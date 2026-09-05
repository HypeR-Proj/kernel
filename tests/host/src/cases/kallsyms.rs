// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Runtime symbol lookup, demangling, and malformed-table rejection.

use hyper::debug::kallsyms::{
    COMPACT_HEADER_SIZE, COMPACT_MAGIC, COMPACT_RECORD_SIZE, COMPACT_VERSION, CompactSymbolTable,
    Error, SymbolTable,
};

fn symbol(name: u32, info: u8, section: u16, value: u64, size: u64) -> [u8; 24] {
    let mut bytes = [0u8; 24];
    bytes[0..4].copy_from_slice(&name.to_le_bytes());
    bytes[4] = info;
    bytes[6..8].copy_from_slice(&section.to_le_bytes());
    bytes[8..16].copy_from_slice(&value.to_le_bytes());
    bytes[16..24].copy_from_slice(&size.to_le_bytes());
    bytes
}

#[test]
fn resolves_the_nearest_preceding_runtime_function() {
    let mut symbols = Vec::new();
    symbols.extend_from_slice(&symbol(0, 0, 0, 0, 0));
    symbols.extend_from_slice(&symbol(1, 2, 1, 0x100, 0x40));
    symbols.extend_from_slice(&symbol(7, 2, 1, 0x180, 0x20));
    let table = crate::require_ok(SymbolTable::new(
        &symbols,
        b"\0first\0second\0",
        0xff00_2000_0000,
        0x1000,
    ));

    let resolved = crate::require_some(crate::require_ok(table.lookup(0xff00_2000_0118)));
    assert_eq!(resolved.name, "first");
    assert_eq!(resolved.address, 0xff00_2000_0100);
    assert_eq!(resolved.size, 0x40);
    assert_eq!(resolved.offset, 0x18);

    let resolved = crate::require_some(crate::require_ok(table.lookup(0xff00_2000_0188)));
    assert_eq!(resolved.name, "second");
    assert_eq!(resolved.offset, 8);
    assert!(crate::require_ok(table.lookup_containing(0xff00_2000_0148)).is_none());
    let contained =
        crate::require_some(crate::require_ok(table.lookup_containing(0xff00_2000_0188)));
    assert_eq!(contained.name, "second");
    assert!(crate::require_ok(table.lookup(0x100)).is_none());
}

#[test]
fn displays_runtime_symbols_with_rust_names_demangled() {
    let symbol = hyper::debug::kallsyms::Symbol {
        name: "_RNvCskwGfYPst2Cb_3foo16example_function",
        address: 0x1000,
        size: 0x40,
        offset: 0x18,
    };
    assert_eq!(symbol.to_string(), "foo::example_function+0x18/0x40");

    let foreign = hyper::debug::kallsyms::Symbol {
        name: "start_kernel",
        address: 0x2000,
        size: 0x20,
        offset: 4,
    };
    assert_eq!(foreign.to_string(), "start_kernel+0x4/0x20");
}

#[test]
fn resolves_a_containing_symbol_from_the_compact_runtime_table() {
    let strings = b"first\0second\0";
    let strings_offset = COMPACT_HEADER_SIZE + 2 * COMPACT_RECORD_SIZE;
    let mut image = vec![0; strings_offset + strings.len()];
    image[..8].copy_from_slice(&COMPACT_MAGIC);
    put_u32(&mut image, 8, COMPACT_VERSION);
    put_u32(&mut image, 12, 2);
    put_u32(&mut image, 16, strings_offset as u32);
    put_u32(&mut image, 20, strings.len() as u32);
    put_record(&mut image, 0, 0x100, 0x40, 0);
    put_record(&mut image, 1, 0x180, 0x20, 6);
    image[strings_offset..].copy_from_slice(strings);

    let base = 0xff00_4000_0000;
    let table = crate::require_ok(CompactSymbolTable::new(&image, base, 0x1000));
    let symbol = crate::require_some(crate::require_ok(table.lookup_containing(base + 0x188)));
    assert_eq!(symbol.name, "second");
    assert_eq!(symbol.offset, 8);
    assert!(crate::require_ok(table.lookup_containing(base + 0x160)).is_none());
}

fn put_record(image: &mut [u8], index: usize, address: u64, size: u32, name: u32) {
    let offset = COMPACT_HEADER_SIZE + index * COMPACT_RECORD_SIZE;
    image[offset..offset + 8].copy_from_slice(&address.to_le_bytes());
    put_u32(image, offset + 8, size);
    put_u32(image, offset + 12, name);
}

fn put_u32(image: &mut [u8], offset: usize, value: u32) {
    image[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

#[test]
fn rejects_malformed_symbol_metadata() {
    assert!(matches!(
        SymbolTable::new(&[0; 23], b"\0", 0, 0x1000),
        Err(Error::InvalidSymbolTable)
    ));
    assert!(matches!(
        SymbolTable::new(&[0; 24], b"bad", 0, 0x1000),
        Err(Error::InvalidStringTable)
    ));
}
