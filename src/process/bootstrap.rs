// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Kernel-owned capabilities and opaque data delivered to the selected loader.

use hyper::abi::native::{HyperNativeLoaderStartup, HyperNativeStartupHandle};

use crate::accounting::ResourceDomain;
use crate::authority::Rights;
use {crate::capability::HandleFlags, crate::capability::PreparedHandle};
use {crate::ipc::ByteChannel, crate::ipc::ByteChannelError, crate::ipc::PreparedByteMessage};

use {crate::object::ObjectPublication, crate::object::diagnostics::PairedObject};

#[derive(Debug)]
pub(crate) enum Error {
    Channel(ByteChannelError),
    Process(super::ProcessError),
}

pub(crate) struct PreparedBootstrap {
    pub(crate) executable: PreparedHandle,
    pub(crate) child_channel: PreparedHandle,
    pub(crate) parent_channel: PreparedHandle,
}

/// All storage is sponsored before publication. Dropping this owner closes
/// both unpublished endpoints and releases the staged File capability.
pub(crate) fn prepare(
    domain: &ResourceDomain,
    executable: &PreparedHandle,
    header: HyperNativeLoaderStartup,
    records: impl Iterator<Item = HyperNativeStartupHandle>,
    data: &[u8],
) -> Result<PreparedBootstrap, Error> {
    let executable = executable
        .try_duplicate(super::builder::EXECUTABLE_RIGHTS)
        .map_err(|error| Error::Process(error.into()))?;

    let mut message = PreparedByteMessage::try_new(
        domain,
        core::mem::size_of::<HyperNativeLoaderStartup>()
            + header.handle_count as usize * 16
            + data.len(),
    )
    .map_err(Error::Channel)?;
    let bytes = message.bytes_mut();
    // Explicit wire encoding avoids padding disclosure and unaligned access.
    for (index, value) in [
        header.size,
        header.handle_count,
        header.data_size,
        header.flags,
    ]
    .into_iter()
    .enumerate()
    {
        bytes[index * 4..index * 4 + 4].copy_from_slice(&value.to_le_bytes());
    }
    for (index, value) in [
        header.executable,
        header.root_vmar,
        header.stack_vmar,
        header.stack_base,
        header.stack_size,
        header.loader_base,
        header.loader_size,
        header.execution_context,
        header.object_factory,
        header.system_info,
    ]
    .into_iter()
    .enumerate()
    {
        bytes[16 + index * 8..24 + index * 8].copy_from_slice(&value.to_le_bytes());
    }
    let mut offset = core::mem::size_of::<HyperNativeLoaderStartup>();
    for record in records {
        bytes[offset..offset + 4].copy_from_slice(&record.purpose.to_le_bytes());
        bytes[offset + 4..offset + 8].copy_from_slice(&record.flags.to_le_bytes());
        bytes[offset + 8..offset + 16].copy_from_slice(&record.handle.to_le_bytes());
        offset += 16;
    }
    bytes[offset..].copy_from_slice(data);
    let (parent, child) = ByteChannel::try_pair(domain).map_err(Error::Channel)?;
    let parent = ObjectPublication::try_new(parent).map_err(|e| Error::Process(e.into()))?;
    let child = ObjectPublication::try_new(child).map_err(|e| Error::Process(e.into()))?;
    parent.object().bind_peer_identity(child.koid());
    child.object().bind_peer_identity(parent.koid());
    parent
        .object()
        .prepare_write(&message)
        .map_err(Error::Channel)?
        .publish(message);
    let parent_channel = PreparedHandle::try_from_new_object(
        parent,
        Rights::READ.union(Rights::WAIT),
        HandleFlags::NONE,
    )
    .map_err(|e| Error::Process(e.into()))?;
    let child_channel = PreparedHandle::try_from_new_object(
        child,
        Rights::READ.union(Rights::WRITE),
        HandleFlags::NONE,
    )
    .map_err(|e| Error::Process(e.into()))?;
    Ok(PreparedBootstrap {
        executable,
        child_channel,
        parent_channel,
    })
}
