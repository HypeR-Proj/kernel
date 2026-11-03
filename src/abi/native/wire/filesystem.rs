// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! File and directory record encoding after backend completion.

use super::{copy_strided_record, write_u32, write_u64};
use crate::abi::native::services::UserMemoryServices;
use crate::fs::vfs::{DirectoryInfo, DirectoryPage, FileInfo};
use crate::mm::user_space::UserSlice;
use hyper::abi::native::{
    HYPER_NATIVE_DIRECTORY_ENTRY_KIND_DIRECTORY, HYPER_NATIVE_DIRECTORY_ENTRY_KIND_FILE,
    HYPER_NATIVE_DIRECTORY_ENTRY_KIND_OTHER, HYPER_NATIVE_DIRECTORY_ENTRY_KIND_SYMLINK,
    HYPER_NATIVE_STATUS_INTERNAL, HyperNativeDirectoryEntry, HyperNativeDirectoryInfo,
    HyperNativeFileInfo, HyperNativeStatus,
};

// Encoding/copyout follows backend completion, outside nested filesystem RPC.
#[inline(never)]
pub(in crate::abi::native) fn copy_directory_page(
    services: &impl UserMemoryServices,
    destination: UserSlice,
    page: &DirectoryPage,
) -> Result<(usize, u64), HyperNativeStatus> {
    let stride = destination.length() as usize
        / hyper::abi::native::HYPER_NATIVE_DIRECTORY_ENTRY_PAGE_CAPACITY as usize;
    for (index, entry) in page.entries().enumerate() {
        copy_strided_record(
            services,
            destination,
            index,
            stride,
            &encode_directory_entry(*entry)?,
        )?;
    }
    Ok((page.len(), page.next_cookie()))
}

pub(in crate::abi::native) fn encode_directory_entry(
    snapshot: crate::fs::vfs::DirectoryEntrySnapshot,
) -> Result<[u8; core::mem::size_of::<HyperNativeDirectoryEntry>()], HyperNativeStatus> {
    const SIZE: usize = core::mem::offset_of!(HyperNativeDirectoryEntry, size);
    const MODE: usize = core::mem::offset_of!(HyperNativeDirectoryEntry, mode);
    const KIND: usize = core::mem::offset_of!(HyperNativeDirectoryEntry, kind);
    const NAME_LENGTH: usize = core::mem::offset_of!(HyperNativeDirectoryEntry, name_length);
    const NAME: usize = core::mem::offset_of!(HyperNativeDirectoryEntry, name);

    let mut record = [0_u8; core::mem::size_of::<HyperNativeDirectoryEntry>()];
    write_u64(&mut record, SIZE, snapshot.attributes.size());
    write_u32(&mut record, MODE, snapshot.attributes.mode());
    write_u32(
        &mut record,
        KIND,
        directory_entry_kind(snapshot.attributes.kind()),
    );
    write_u32(&mut record, NAME_LENGTH, snapshot.name_length);
    let name_length =
        usize::try_from(snapshot.name_length).map_err(|_| HYPER_NATIVE_STATUS_INTERNAL)?;
    let source = snapshot
        .name
        .get(..name_length)
        .ok_or(HYPER_NATIVE_STATUS_INTERNAL)?;
    let end = NAME
        .checked_add(name_length)
        .ok_or(HYPER_NATIVE_STATUS_INTERNAL)?;
    let destination = record
        .get_mut(NAME..end)
        .ok_or(HYPER_NATIVE_STATUS_INTERNAL)?;
    destination.copy_from_slice(source);
    Ok(record)
}

pub(in crate::abi::native) const fn directory_entry_kind(
    kind: crate::fs::primitives::NodeKind,
) -> u32 {
    match kind {
        crate::fs::primitives::NodeKind::File => HYPER_NATIVE_DIRECTORY_ENTRY_KIND_FILE as u32,
        crate::fs::primitives::NodeKind::Directory => {
            HYPER_NATIVE_DIRECTORY_ENTRY_KIND_DIRECTORY as u32
        }
        crate::fs::primitives::NodeKind::Symlink => {
            HYPER_NATIVE_DIRECTORY_ENTRY_KIND_SYMLINK as u32
        }
        crate::fs::primitives::NodeKind::Other => HYPER_NATIVE_DIRECTORY_ENTRY_KIND_OTHER as u32,
    }
}

pub(in crate::abi::native) fn encode_file_info(
    info: FileInfo,
) -> [u8; core::mem::size_of::<HyperNativeFileInfo>()] {
    encode_file_info_fields(
        info.location.filesystem_id,
        info.location.mount_id,
        info.location.node_id,
        info.size,
        info.mode,
    )
}

pub(in crate::abi::native) fn encode_file_info_fields(
    filesystem_id: u64,
    mount_id: u64,
    node_id: u64,
    size: u64,
    mode: u32,
) -> [u8; core::mem::size_of::<HyperNativeFileInfo>()] {
    let mut record = [0_u8; core::mem::size_of::<HyperNativeFileInfo>()];
    write_u64(
        &mut record,
        core::mem::offset_of!(HyperNativeFileInfo, filesystem_id),
        filesystem_id,
    );
    write_u64(
        &mut record,
        core::mem::offset_of!(HyperNativeFileInfo, mount_id),
        mount_id,
    );
    write_u64(
        &mut record,
        core::mem::offset_of!(HyperNativeFileInfo, node_id),
        node_id,
    );
    write_u64(
        &mut record,
        core::mem::offset_of!(HyperNativeFileInfo, size),
        size,
    );
    write_u32(
        &mut record,
        core::mem::offset_of!(HyperNativeFileInfo, mode),
        mode,
    );
    record
}

pub(in crate::abi::native) fn encode_directory_info(
    info: DirectoryInfo,
) -> [u8; core::mem::size_of::<HyperNativeDirectoryInfo>()] {
    encode_directory_info_fields(
        info.location.filesystem_id,
        info.location.mount_id,
        info.location.node_id,
        info.mode,
    )
}

pub(in crate::abi::native) fn encode_directory_info_fields(
    filesystem_id: u64,
    mount_id: u64,
    node_id: u64,
    mode: u32,
) -> [u8; core::mem::size_of::<HyperNativeDirectoryInfo>()] {
    let mut record = [0_u8; core::mem::size_of::<HyperNativeDirectoryInfo>()];
    write_u64(
        &mut record,
        core::mem::offset_of!(HyperNativeDirectoryInfo, filesystem_id),
        filesystem_id,
    );
    write_u64(
        &mut record,
        core::mem::offset_of!(HyperNativeDirectoryInfo, mount_id),
        mount_id,
    );
    write_u64(
        &mut record,
        core::mem::offset_of!(HyperNativeDirectoryInfo, node_id),
        node_id,
    );
    write_u32(
        &mut record,
        core::mem::offset_of!(HyperNativeDirectoryInfo, mode),
        mode,
    );
    record
}
