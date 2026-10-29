// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Bounded user input snapshots for privileged namespace transactions.

use crate::fs::vfs::service::copy_path;
use crate::process::Process;
use hyper::abi::native as abi;
use {
    crate::capability::HandleValue, crate::capability::InvocationReceiver,
    crate::capability::Rights,
};
use {
    crate::fs::vfs::DirectoryObject, crate::fs::vfs::Error as VfsError,
    crate::fs::vfs::ScratchBudget, crate::fs::vfs::ScratchVec, crate::fs::vfs::VfsServiceError,
};
use {crate::mm::user_space::UserAddress, crate::mm::user_space::UserSlice};

const NAMESPACE_RIGHTS: Rights = Rights::MANAGE_NAMESPACE
    .union(Rights::WRITE)
    .union(Rights::EXECUTE);
const MAX_MOVES: usize = abi::HYPER_NATIVE_FILESYSTEM_SWITCH_ROOT_MAX_MOVES as usize;

pub(super) fn mount_ramfs(
    process: &Process,
    root: &InvocationReceiver,
    path: UserSlice,
) -> Result<(), VfsServiceError> {
    let directory = root
        .resolve::<DirectoryObject>(NAMESPACE_RIGHTS)
        .map_err(crate::process::ProcessError::from)?;
    let path = copy_path(process, path)?;
    directory
        .object()
        .mount_ramfs(&path, &process.resource_domain())
        .map_err(Into::into)
}

pub(super) fn switch_root(
    process: &Process,
    root: &InvocationReceiver,
    request: &[u8; 48],
) -> Result<(), VfsServiceError> {
    with_root_request(
        process,
        root,
        request,
        |directory, new_root, put_old, moves| {
            directory
                .switch_root(new_root, put_old, moves, &process.resource_domain())
                .map_err(Into::into)
        },
    )
}

pub(super) fn begin_root_switch(
    process: &Process,
    root: &InvocationReceiver,
    request: &[u8; 48],
) -> Result<HandleValue, VfsServiceError> {
    with_root_request(
        process,
        root,
        request,
        |directory, new_root, put_old, moves| {
            use {
                crate::capability::HandleFlags, crate::capability::PreparedHandle,
                crate::fs::vfs::RootTransitionObject, crate::object::KernelObject,
                crate::object::ObjectPublication, crate::process::ProcessError,
            };
            let transition = directory.prepare_root_transition(
                new_root,
                put_old,
                moves,
                &process.resource_domain(),
            )?;
            let transition = hyper::mm::FallibleArc::try_new(transition)
                .map_err(|_| VfsServiceError::Process(ProcessError::Allocation))?;
            let object =
                RootTransitionObject::try_new(transition.clone(), &process.resource_domain())?;
            let publication = ObjectPublication::try_new(object).map_err(ProcessError::from)?;
            let prepared = PreparedHandle::try_from_new_object(
                publication,
                RootTransitionObject::SUPPORTED_RIGHTS,
                HandleFlags::NONE,
            )
            .map_err(ProcessError::from)?;
            let reservation = process.reserve_handles::<1>()?;
            if let Err(error) = transition.activate() {
                process.abort_handles(reservation);
                return Err(error.into());
            }
            match process.publish_handles(reservation, [prepared]) {
                Ok([handle]) => Ok(handle),
                Err(failure) => {
                    // An operation pin may outlive failed publication. Rollback is
                    // explicit here, before returning a failed syscall to userspace.
                    transition.abort();
                    Err(VfsServiceError::Process(failure.error))
                }
            }
        },
    )
}

pub(super) fn finish_root_switch(
    token: &InvocationReceiver,
    commit: bool,
) -> Result<(), VfsServiceError> {
    let token = token
        .resolve::<crate::fs::vfs::RootTransitionObject>(Rights::WRITE)
        .map_err(crate::process::ProcessError::from)?;
    token.object().finish(commit).map_err(Into::into)
}

fn with_root_request<T>(
    process: &Process,
    root: &InvocationReceiver,
    request: &[u8; 48],
    operation: impl FnOnce(&DirectoryObject, &str, &str, &[(&str, &str)]) -> Result<T, VfsServiceError>,
) -> Result<T, VfsServiceError> {
    let directory = root
        .resolve::<DirectoryObject>(NAMESPACE_RIGHTS)
        .map_err(crate::process::ProcessError::from)?;
    let words = decode_words::<6>(request)?;
    let count = usize::try_from(words[5]).map_err(|_| VfsServiceError::InvalidInput)?;
    if count > MAX_MOVES || (count == 0 && words[4] != 0) {
        return Err(VfsServiceError::InvalidInput);
    }
    let new_root = copy_path(process, user_slice(words[0], words[1])?)?;
    let put_old = copy_path(process, user_slice(words[2], words[3])?)?;
    let mut wire_moves = [0u8; MAX_MOVES * 32];
    let bytes = count.checked_mul(32).ok_or(VfsServiceError::InvalidInput)?;
    if count != 0 {
        process.copy_from_user(
            user_slice(words[4], bytes as u64)?,
            &mut wire_moves[..bytes],
        )?;
    }
    // Path owners include quota bookkeeping. Keep the variable-sized set off
    // the syscall stack, which remains live while remote lookup can block.
    let mut owned = ScratchVec::new(ScratchBudget::new(&process.resource_domain()));
    owned.try_reserve_exact(count).map_err(VfsError::from)?;
    for record in wire_moves[..bytes].chunks_exact(32) {
        let fields = decode_words::<4>(record)?;
        owned
            .push((
                copy_path(process, user_slice(fields[0], fields[1])?)?,
                copy_path(process, user_slice(fields[2], fields[3])?)?,
            ))
            .map_err(VfsError::from)?;
    }
    let mut moves = [("", ""); MAX_MOVES];
    for (output, (source, target)) in moves.iter_mut().zip(owned.iter()) {
        *output = (source.as_str(), target.as_str());
    }
    // All fallible user reads are complete before the VFS prepares or publishes
    // topology. Concurrent userspace writes cannot replace paths after validation.
    operation(directory.object(), &new_root, &put_old, &moves[..count])
}

fn user_slice(address: u64, length: u64) -> Result<UserSlice, VfsServiceError> {
    UserSlice::new(UserAddress::new(address), length).map_err(|_| VfsServiceError::InvalidInput)
}

fn decode_words<const N: usize>(bytes: &[u8]) -> Result<[u64; N], VfsServiceError> {
    if bytes.len() != N * 8 {
        return Err(VfsServiceError::InvalidInput);
    }
    let mut words = [0; N];
    for (word, bytes) in words.iter_mut().zip(bytes.chunks_exact(8)) {
        *word = u64::from_le_bytes(
            bytes
                .try_into()
                .map_err(|_| VfsServiceError::InvalidInput)?,
        );
    }
    Ok(words)
}
