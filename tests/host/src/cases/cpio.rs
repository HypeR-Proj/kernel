// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! CPIO parsing and immutable ramfs contracts.

use hyper::archive::cpio::{Archive, EntryKind, Error};
use hyper::fs::NodeKind;
use hyper::fs::ramfs::{Error as RamFsError, RamFs};

fn append_hex(output: &mut Vec<u8>, value: u32) {
    output.extend_from_slice(format!("{value:08x}").as_bytes());
}

fn append_entry(output: &mut Vec<u8>, name: &str, mode: u32, data: &[u8]) {
    append_entry_with_checksum(output, name, mode, data, false);
}

fn append_entry_with_checksum(
    output: &mut Vec<u8>,
    name: &str,
    mode: u32,
    data: &[u8],
    checksum: bool,
) {
    output.extend_from_slice(if checksum { b"070702" } else { b"070701" });
    append_hex(output, 1);
    append_hex(output, mode);
    for value in [0, 0, 1, 0] {
        append_hex(output, value);
    }
    append_hex(output, data.len() as u32);
    for value in [0, 0, 0, 0] {
        append_hex(output, value);
    }
    append_hex(output, (name.len() + 1) as u32);
    append_hex(
        output,
        if checksum {
            data.iter()
                .fold(0u32, |sum, byte| sum.wrapping_add(u32::from(*byte)))
        } else {
            0
        },
    );
    output.extend_from_slice(name.as_bytes());
    output.push(0);
    while output.len() & 3 != 0 {
        output.push(0);
    }
    output.extend_from_slice(data);
    while output.len() & 3 != 0 {
        output.push(0);
    }
}

fn archive(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut output = Vec::new();
    for (name, data) in entries {
        append_entry(&mut output, name, 0o100_644, data);
    }
    append_entry(&mut output, "TRAILER!!!", 0, &[]);
    output
}

fn archive_with_modes(entries: &[(&str, u32, &[u8])]) -> Vec<u8> {
    let mut output = Vec::new();
    for (name, mode, data) in entries {
        append_entry(&mut output, name, *mode, data);
    }
    append_entry(&mut output, "TRAILER!!!", 0, &[]);
    output
}

#[test]
fn parses_newc_files_without_allocating_in_the_parser() {
    let bytes = archive(&[("hypervisor/boot.conf", b"default=demo"), ("empty", b"")]);
    let archive = crate::require_ok(Archive::new(&bytes));
    let entry = crate::require_some(crate::require_ok(
        archive.find_unique("hypervisor/boot.conf"),
    ));
    assert_eq!(entry.kind(), EntryKind::File);
    assert_eq!(entry.data(), b"default=demo");
    assert_eq!(crate::require_ok(archive.find_unique("missing")), None);
}

#[test]
fn rejects_duplicate_and_truncated_entries() {
    let duplicate = archive(&[("manifest", b"one"), ("manifest", b"two")]);
    let parsed = crate::require_ok(Archive::new(&duplicate));
    assert_eq!(parsed.find_unique("manifest"), Err(Error::DuplicateEntry));

    let mut truncated = archive(&[("manifest", b"payload")]);
    truncated.truncate(truncated.len() - 120);
    assert!(Archive::new(&truncated).is_err());
}

#[test]
fn validates_crc_archives() {
    let mut bytes = Vec::new();
    append_entry_with_checksum(&mut bytes, "payload", 0o100_644, b"checked", true);
    append_entry(&mut bytes, "TRAILER!!!", 0, &[]);
    let parsed = crate::require_ok(Archive::new(&bytes));
    let payload = crate::require_some(crate::require_ok(parsed.find_unique("payload")));
    assert_eq!(payload.data(), b"checked");

    let data_offset = payload.data().as_ptr() as usize - bytes.as_ptr() as usize;
    bytes[data_offset] ^= 1;
    assert_eq!(
        Archive::new(&bytes).map(|_| ()),
        Err(Error::InvalidChecksum)
    );

    let mut newc_with_checksum = archive(&[("payload", b"unchecked")]);
    newc_with_checksum[102..110].copy_from_slice(b"00000001");
    assert_eq!(
        Archive::new(&newc_with_checksum).map(|_| ()),
        Err(Error::InvalidChecksum)
    );
}

#[test]
fn mounts_an_immutable_root_filesystem_with_canonical_lookup() {
    let bytes = archive_with_modes(&[
        (".", 0o040_755, b""),
        ("etc", 0o040_755, b""),
        ("etc/config", 0o100_644, b"value"),
        ("./init", 0o100_755, b"native image"),
    ]);
    let root = crate::require_ok(RamFs::from_newc(&bytes));

    assert_eq!(root.nodes().len(), 4);
    assert_eq!(root.root().path(), "/");
    assert_eq!(root.root().kind(), NodeKind::Directory);
    let init = crate::require_some(crate::require_ok(root.lookup("/init")));
    assert_eq!(init.kind(), NodeKind::File);
    assert!(init.is_executable());
    assert_eq!(init.data(), b"native image");
    let config = crate::require_some(crate::require_ok(root.lookup("/etc/config")));
    assert_eq!(config.data(), b"value");
    assert_eq!(crate::require_ok(root.lookup("/missing")), None);
}

#[test]
fn rejects_ambiguous_or_escaping_ramfs_paths() {
    let duplicate = archive(&[("init", b"one"), ("./init", b"two")]);
    assert_eq!(
        RamFs::from_newc(&duplicate).map(|_| ()),
        Err(RamFsError::DuplicatePath)
    );

    for path in ["/absolute", "../escape", "dir/../escape", "dir//file"] {
        let bytes = archive(&[(path, b"payload")]);
        assert_eq!(
            RamFs::from_newc(&bytes).map(|_| ()),
            Err(RamFsError::InvalidPath)
        );
    }

    let root_file = archive_with_modes(&[(".", 0o100_755, b"payload")]);
    assert_eq!(
        RamFs::from_newc(&root_file).map(|_| ()),
        Err(RamFsError::InvalidPath)
    );
}
