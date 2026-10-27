// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Filesystem record validation without dereferencing real userspace addresses.

use super::{
    INVALID, METADATA_SIZE, Metadata, MetadataUpdate, Timestamp, UserMemoryServices,
    copy_info_record, decode_metadata_update, encode_metadata, metadata_request,
};
use crate::fs::vfs::NodeLocationInfo;
use crate::mm::user_space::UserSlice;
use crate::process::ProcessError;
use core::cell::{Cell, RefCell};
use hyper::abi::native as abi;
use {crate::fs::primitives::NodeAttributes, crate::fs::primitives::NodeKind};

const BASE: u64 = 0x3000;

struct Memory {
    bytes: RefCell<[u8; 256]>,
    copies: Cell<usize>,
    fail: Cell<bool>,
}

impl Memory {
    const fn new() -> Self {
        Self {
            bytes: RefCell::new([0; 256]),
            copies: Cell::new(0),
            fail: Cell::new(false),
        }
    }

    fn reset(&self) {
        self.bytes.borrow_mut().fill(0);
        self.copies.set(0);
        self.fail.set(false);
    }

    fn word(&self, offset: usize, value: u32) {
        self.bytes.borrow_mut()[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn long(&self, offset: usize, value: i64) {
        self.bytes.borrow_mut()[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
    }
}

impl UserMemoryServices for Memory {
    fn copy_to_user(&self, destination: UserSlice, source: &[u8]) -> Result<(), ProcessError> {
        if self.fail.get() {
            return Err(ProcessError::Allocation);
        }
        let mut bytes = self.bytes.borrow_mut();
        let begin = destination
            .base()
            .get()
            .checked_sub(BASE)
            .ok_or(ProcessError::Allocation)? as usize;
        let end = begin
            .checked_add(source.len())
            .ok_or(ProcessError::Allocation)?;
        bytes
            .get_mut(begin..end)
            .ok_or(ProcessError::Allocation)?
            .copy_from_slice(source);
        self.copies.set(self.copies.get() + 1);
        Ok(())
    }

    fn copy_from_user(
        &self,
        source: UserSlice,
        destination: &mut [u8],
    ) -> Result<(), ProcessError> {
        if self.fail.get() {
            return Err(ProcessError::Allocation);
        }
        let bytes = self.bytes.borrow();
        let begin = source
            .base()
            .get()
            .checked_sub(BASE)
            .ok_or(ProcessError::Allocation)? as usize;
        let end = begin
            .checked_add(destination.len())
            .ok_or(ProcessError::Allocation)?;
        destination.copy_from_slice(bytes.get(begin..end).ok_or(ProcessError::Allocation)?);
        self.copies.set(self.copies.get() + 1);
        Ok(())
    }
}

pub(crate) struct Error(u32);

impl core::fmt::Debug for Error {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_tuple("FilesystemWireContract")
            .field(&self.0)
            .finish()
    }
}

pub(crate) fn run() -> Result<(), Error> {
    let memory = Memory::new();
    for size in [0, 39, abi::HYPER_NATIVE_EXTENSIBLE_RECORD_MAX_BYTES + 1] {
        memory.reset();
        if decode_metadata_update(&memory, BASE, size) != Err(INVALID) || memory.copies.get() != 0 {
            return Err(Error(1));
        }
    }
    for size in [40, 41, 104, 256] {
        memory.reset();
        if decode_metadata_update(&memory, BASE, size) != Ok(MetadataUpdate::default()) {
            return Err(Error(2));
        }
    }
    // Check every extension chunk, not merely the first byte beyond the prefix.
    for offset in [40, 103, 104, 255] {
        memory.reset();
        memory.bytes.borrow_mut()[offset] = 1;
        if decode_metadata_update(&memory, BASE, 256) != Err(INVALID) {
            return Err(Error(3));
        }
    }
    for (offset, value) in [
        (0, 8),
        (4, 0o1000),
        (4, 0o444),
        (20, 1),
        (36, 1),
        (8, 1),
        (24, 1),
    ] {
        memory.reset();
        memory.word(offset, value);
        if decode_metadata_update(&memory, BASE, 40) != Err(INVALID) {
            return Err(Error(4));
        }
    }
    memory.reset();
    memory.word(0, 7);
    memory.word(4, 0o640);
    memory.long(8, -1);
    memory.word(16, 999_999_999);
    memory.long(24, i64::MIN);
    let update = decode_metadata_update(&memory, BASE, 40).map_err(|_| Error(5))?;
    if update.mode != Some(0o640)
        || update.accessed != Timestamp::new(-1, 999_999_999)
        || update.modified != Timestamp::new(i64::MIN, 0)
    {
        return Err(Error(6));
    }
    memory.word(16, 1_000_000_000);
    if decode_metadata_update(&memory, BASE, 40) != Err(INVALID) {
        return Err(Error(7));
    }
    memory.word(16, 0);
    memory.word(32, u32::MAX);
    if decode_metadata_update(&memory, BASE, 40) != Err(INVALID) {
        return Err(Error(8));
    }
    memory.fail.set(true);
    if decode_metadata_update(&memory, BASE, 40) != Err(abi::HYPER_NATIVE_STATUS_NO_MEMORY) {
        return Err(Error(9));
    }
    check_output(&memory)
}

fn check_output(memory: &Memory) -> Result<(), Error> {
    // Metadata and directory entries share the published kind numbering.
    // In particular, Other is a valid kind, not an unknown zero sentinel.
    for (kind, encoded) in [
        (NodeKind::File, abi::HYPER_NATIVE_DIRECTORY_ENTRY_KIND_FILE),
        (
            NodeKind::Directory,
            abi::HYPER_NATIVE_DIRECTORY_ENTRY_KIND_DIRECTORY,
        ),
        (
            NodeKind::Symlink,
            abi::HYPER_NATIVE_DIRECTORY_ENTRY_KIND_SYMLINK,
        ),
        (
            NodeKind::Other,
            abi::HYPER_NATIVE_DIRECTORY_ENTRY_KIND_OTHER,
        ),
    ] {
        let record = encode_metadata(Metadata {
            location: NodeLocationInfo {
                filesystem_id: 7,
                mount_id: 8,
                node_id: 9,
            },
            attributes: NodeAttributes::new(kind, 0o640, 123),
            accessed: None,
            modified: None,
            created: None,
            changed: None,
        });
        let offset = core::mem::offset_of!(abi::HyperNativeFileMetadata, kind);
        if record[offset..offset + 4] != (encoded as u32).to_le_bytes() {
            return Err(Error(17));
        }
    }
    let bytes = encode_metadata(Metadata {
        location: NodeLocationInfo {
            filesystem_id: 7,
            mount_id: 8,
            node_id: 9,
        },
        attributes: NodeAttributes::new(NodeKind::File, 0o640, 123),
        accessed: Timestamp::new(-1, 999_999_999),
        modified: None,
        created: Timestamp::new(0, 0),
        changed: Timestamp::new(i64::MAX, 123),
    });
    let mut expected = [0; 112];
    for (offset, value) in [
        (0, 7_u64),
        (8, 8),
        (16, 9),
        (24, 123),
        (48, u64::MAX),
        (96, i64::MAX as u64),
    ] {
        expected[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
    }
    for (offset, value) in [
        (32, 0o640_u32),
        (36, 1),
        (40, 13),
        (56, 999_999_999),
        (104, 123),
    ] {
        expected[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }
    if bytes != expected {
        return Err(Error(10));
    }
    for capacity in [0, 111, abi::HYPER_NATIVE_EXTENSIBLE_RECORD_MAX_BYTES + 1] {
        if metadata_request(BASE, capacity).is_ok() {
            return Err(Error(11));
        }
    }
    for capacity in [112, 256, abi::HYPER_NATIVE_EXTENSIBLE_RECORD_MAX_BYTES] {
        memory.reset();
        memory.bytes.borrow_mut().fill(0xa5);
        let request = metadata_request(BASE, capacity).map_err(|_| Error(12))?;
        if copy_info_record(memory, request, &bytes) != Ok(METADATA_SIZE as u64) {
            return Err(Error(13));
        }
        let written = memory.bytes.borrow();
        if written[..112] != expected || written[112..].iter().any(|byte| *byte != 0xa5) {
            return Err(Error(14));
        }
    }
    memory.fail.set(true);
    let request = metadata_request(BASE, 112).map_err(|_| Error(15))?;
    if copy_info_record(memory, request, &bytes) != Err(abi::HYPER_NATIVE_STATUS_NO_MEMORY) {
        return Err(Error(16));
    }
    Ok(())
}
