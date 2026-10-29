// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Process-facing VFS operations and user-memory validation.

use crate::authority::Rights;
use crate::object::ObjectPublication;
use {
    crate::capability::HandleFlags, crate::capability::HandleValue,
    crate::capability::InvocationReceiver, crate::capability::PreparedHandle,
};
use {crate::mm::user_space::UserSlice, crate::mm::user_space::UserWriteReservation};
use {crate::process::Process, crate::process::ProcessError};

use super::{
    DirectoryObject, DirectoryPage, Error as VfsError, FileObject, ScratchBudget, ScratchString,
    ScratchVec,
};

const MAX_PATH_BYTES: usize = hyper::abi::native::HYPER_NATIVE_DIRECTORY_MAX_PATH_BYTES as usize;
const MAX_READ_BYTES: usize = hyper::abi::native::HYPER_NATIVE_FILE_MAX_READ_BYTES as usize;
const SMALL_TRANSFER_BYTES: usize = 1024;
const BULK_TRANSFER_BYTES: usize = 512 * 1024;

#[derive(Debug)]
pub(crate) enum ServiceError {
    FileLock(super::locks::LockError),
    FileSystem(VfsError),
    InvalidInput,
    Process(ProcessError),
}

impl From<VfsError> for ServiceError {
    fn from(error: VfsError) -> Self {
        Self::FileSystem(error)
    }
}

impl From<ProcessError> for ServiceError {
    fn from(error: ProcessError) -> Self {
        Self::Process(error)
    }
}

pub(crate) fn open_directory(
    process: &Process,
    directory: &InvocationReceiver,
    path: UserSlice,
    rights: Rights,
) -> Result<HandleValue, ServiceError> {
    let path = copy_path(process, path)?;
    let required = super::rights_contract::directory_rights_for_directory(rights.bits())
        .and_then(Rights::from_bits)
        .ok_or(ServiceError::InvalidInput)?;
    let directory = directory
        .resolve::<DirectoryObject>(required)
        .map_err(crate::process::ProcessError::from)?;
    let child = directory
        .object()
        .open_directory(&path, &process.resource_domain())?;
    Ok(process.create_object(child, rights)?)
}

/// Create independent iteration authority using only the source Directory's READ right.
pub(crate) fn begin_directory_read(
    process: &Process,
    directory: &InvocationReceiver,
) -> Result<HandleValue, ServiceError> {
    let directory = directory
        .resolve::<DirectoryObject>(Rights::READ)
        .map_err(crate::process::ProcessError::from)?;
    let rights = Rights::from_bits(super::rights_contract::directory_stream_rights(
        directory.rights().bits(),
    ))
    .ok_or(ServiceError::InvalidInput)?;
    let stream = directory.object().begin_read(&process.resource_domain())?;
    Ok(process.create_object(stream, rights)?)
}

pub(crate) fn read_directory(
    directory: &InvocationReceiver,
    cookie: u64,
    page: &mut DirectoryPage,
) -> Result<(), ServiceError> {
    let directory = directory
        .resolve::<super::DirectoryStream>(Rights::READ)
        .map_err(crate::process::ProcessError::from)?;
    directory
        .object()
        .read_page(cookie, page)
        .map_err(Into::into)
}

pub(crate) fn directory_info(
    directory: &InvocationReceiver,
) -> Result<super::DirectoryInfo, ServiceError> {
    let directory = directory
        .resolve::<DirectoryObject>(Rights::INSPECT)
        .map_err(crate::process::ProcessError::from)?;
    directory.object().info().map_err(Into::into)
}

pub(crate) fn file_info(file: &InvocationReceiver) -> Result<super::FileInfo, ServiceError> {
    let file = file
        .resolve::<FileObject>(Rights::INSPECT)
        .map_err(crate::process::ProcessError::from)?;
    file.object().info().map_err(Into::into)
}

/// Explicit cache fill does not advance the per-open sequential predictor or
/// schedule another speculative job. Dropping the result releases every pin.
pub(crate) fn prefetch_file(
    file: &InvocationReceiver,
    offset: u64,
    length: u64,
) -> Result<u64, ServiceError> {
    let file = file
        .resolve::<FileObject>(Rights::READ)
        .map_err(crate::process::ProcessError::from)?;
    if length > hyper::abi::native::HYPER_NATIVE_FILE_MAX_PREFETCH_BYTES
        || offset.checked_add(length).is_none()
    {
        return Err(ServiceError::InvalidInput);
    }
    file.object()
        .prefetch(offset, length as usize)
        .map_err(Into::into)
}

/// Publish one process-local protection token. Object publication checks that
/// the process still admits handles; a concurrent exit rolls preparation back.
pub(crate) fn protect_file(
    process: &Process,
    file: &InvocationReceiver,
) -> Result<(HandleValue, u64), ServiceError> {
    let file = file
        .resolve::<FileObject>(Rights::PROTECT)
        .map_err(crate::process::ProcessError::from)?;
    let domain = process.resource_domain();
    let protection = file.object().protect(&domain)?;
    let size = protection.size();
    let object = super::FileProtectionObject::try_new(protection, &domain)?;
    let handle = process.create_object(object, Rights::MAP.union(Rights::INSPECT))?;
    Ok((handle, size))
}

pub(crate) fn copy_path(process: &Process, path: UserSlice) -> Result<ScratchString, ServiceError> {
    let length = usize::try_from(path.length()).map_err(|_| ServiceError::InvalidInput)?;
    if length == 0 || length > MAX_PATH_BYTES {
        return Err(ServiceError::InvalidInput);
    }
    let mut path_bytes = ScratchVec::new(ScratchBudget::new(&process.resource_domain()));
    super::scratch::resize_bytes(&mut path_bytes, length).map_err(VfsError::from)?;
    process.copy_from_user(path, &mut path_bytes)?;
    if path_bytes.contains(&0) {
        return Err(ServiceError::InvalidInput);
    }
    ScratchString::from_utf8(path_bytes).map_err(|_| ServiceError::InvalidInput)
}

pub(crate) fn read_file_at(
    process: &Process,
    file: &InvocationReceiver,
    offset: u64,
    output: Option<UserSlice>,
) -> Result<(u64, u64), ServiceError> {
    let file = file
        .resolve::<FileObject>(Rights::READ)
        .map_err(crate::process::ProcessError::from)?;
    let file_size = file.object().len()?;
    let Some(output) = output else {
        return Ok((0, file_size));
    };
    let capacity = usize::try_from(output.length()).map_err(|_| ServiceError::InvalidInput)?;
    if capacity > MAX_READ_BYTES {
        return Err(ServiceError::InvalidInput);
    }
    // Bound work by the observed EOF, but recheck through the backend on each
    // batch: concurrent truncation may make a later batch short.
    let capacity = capacity.min(file_size.saturating_sub(offset).min(capacity as u64) as usize);
    if capacity == 0 {
        return Ok((0, file_size));
    }
    // Bulk cached reads retain page owners, then copy only requested bytes into the
    // reserved user range after releasing the content gate. Scratch remains a
    // bounded path for small transfers, RAM files and cache pressure.
    let mut small = [0_u8; SMALL_TRANSFER_BYTES];
    let mut large = ScratchVec::new(ScratchBudget::new(&process.resource_domain()));
    let completed = super::read_contract::read_batches(
        offset,
        capacity,
        BULK_TRANSFER_BYTES,
        |file_offset, completed, length| -> Result<usize, ServiceError> {
            let base = output
                .base()
                .checked_add(completed as u64)
                .ok_or(ServiceError::InvalidInput)?;
            if length >= hyper::mm::PAGE_SIZE as usize
                && let Some(pages) = file.object().read_pages(file_offset, length)?
            {
                if pages.length == 0 {
                    return Ok(0);
                }
                let destination = UserSlice::new(base, pages.length as u64)
                    .map_err(|error| ProcessError::UserMemory(error.into()))?;
                let write = process.reserve_user_write(destination)?;
                write
                    .copy_parts_from(pages.pages())
                    .map_err(ProcessError::UserMemory)?;
                write.complete();
                return Ok(pages.length);
            }
            let bytes = if length <= small.len() {
                &mut small[..length]
            } else {
                super::scratch::resize_bytes(&mut large, length).map_err(VfsError::from)?;
                &mut large[..length]
            };
            let actual = file.object().read(file_offset, &mut bytes[..length])?;
            if actual == 0 {
                return Ok(0);
            }
            let source = bytes
                .get(..actual)
                .filter(|_| actual <= length)
                .ok_or(ServiceError::InvalidInput)?;
            let destination = UserSlice::new(base, actual as u64)
                .map_err(|error| ProcessError::UserMemory(error.into()))?;
            // The backend has released its file lock before COW/page preparation.
            // Pin only this batch; mapping changes elsewhere remain possible.
            let write: UserWriteReservation = process.reserve_user_write(destination)?;
            write.copy_from(source).map_err(ProcessError::UserMemory)?;
            write.complete();
            Ok(actual)
        },
    )
    .map_err(|error| match error {
        super::read_contract::BatchError::Transfer(error) => error,
        super::read_contract::BatchError::Contract(_) => ServiceError::InvalidInput,
    })?;
    Ok((completed as u64, file_size))
}

impl From<super::instance::Error> for ServiceError {
    fn from(error: super::instance::Error) -> Self {
        Self::FileSystem(error.into())
    }
}

pub(crate) fn create_directory(
    process: &Process,
    directory: &InvocationReceiver,
    path: UserSlice,
    mode: u32,
) -> Result<(), ServiceError> {
    let path = copy_path(process, path)?;
    if mode & !0o777 != 0 {
        return Err(ServiceError::InvalidInput);
    }
    let directory = directory
        .resolve::<DirectoryObject>(Rights::READ.union(Rights::WRITE))
        .map_err(crate::process::ProcessError::from)?;
    directory
        .object()
        .create(
            &path,
            crate::fs::primitives::NodeKind::Directory,
            mode,
            &ScratchBudget::new(&process.resource_domain()),
        )
        .map_err(Into::into)
}

pub(crate) fn remove_entry(
    process: &Process,
    directory: &InvocationReceiver,
    path: UserSlice,
    is_directory: bool,
) -> Result<(), ServiceError> {
    let path = copy_path(process, path)?;
    let directory = directory
        .resolve::<DirectoryObject>(Rights::READ.union(Rights::WRITE))
        .map_err(crate::process::ProcessError::from)?;
    directory
        .object()
        .remove(
            &path,
            if is_directory {
                crate::fs::primitives::NodeKind::Directory
            } else {
                crate::fs::primitives::NodeKind::File
            },
            &ScratchBudget::new(&process.resource_domain()),
        )
        .map_err(Into::into)
}

pub(crate) fn resize_file(file: &InvocationReceiver, length: u64) -> Result<(), ServiceError> {
    let file = file
        .resolve::<FileObject>(Rights::WRITE)
        .map_err(crate::process::ProcessError::from)?;
    file.object().resize(length).map_err(Into::into)
}

pub(crate) fn write_file_at(
    process: &Process,
    file: &InvocationReceiver,
    offset: Option<u64>,
    input: Option<UserSlice>,
) -> Result<(u64, u64), ServiceError> {
    let file = file
        .resolve::<FileObject>(Rights::WRITE)
        .map_err(crate::process::ProcessError::from)?;
    let Some(input) = input else {
        return Ok((0, offset.unwrap_or(file.object().len()?)));
    };
    let size = usize::try_from(input.length()).map_err(|_| ServiceError::InvalidInput)?;
    if size > MAX_READ_BYTES {
        return Err(ServiceError::InvalidInput);
    }
    // Resolve all source faults before acquiring content/transport locks. A
    // retained read plan pins only this syscall's source pages, not a writable
    // alias in the filesystem or I/O VM. Publication follows the boundary copy.
    let size = size.min(BULK_TRANSFER_BYTES);
    let source =
        UserSlice::new(input.base(), size as u64).map_err(|_| ServiceError::InvalidInput)?;
    let prepared = process.prepare_user_read(source)?;
    let (actual, end) = file.object().write_source(
        offset,
        &super::write_source::WriteSource::User(&prepared, &process.resource_domain()),
    )?;
    Ok((actual as u64, end))
}

pub(crate) fn directory_scope_create(
    process: &Process,
    root: &InvocationReceiver,
    start: HandleValue,
    rights: Rights,
) -> Result<HandleValue, ServiceError> {
    let required = super::rights_contract::directory_rights_for_directory(
        rights.bits() & !Rights::MANAGE_NAMESPACE.bits(),
    )
    .and_then(Rights::from_bits)
    .ok_or(ServiceError::InvalidInput)?
    .union(rights.intersection(Rights::MANAGE_NAMESPACE));
    let root = root
        .resolve::<DirectoryObject>(required)
        .map_err(crate::process::ProcessError::from)?;
    let start = process.resolve_handle::<DirectoryObject>(start, required)?;
    let scope = DirectoryObject::scope(root.object(), start.object(), &process.resource_domain())?;
    Ok(process.create_object(scope, rights)?)
}

pub(crate) fn directory_get_metadata(
    process: &Process,
    directory: &InvocationReceiver,
    path: UserSlice,
    follow: bool,
) -> Result<super::Metadata, ServiceError> {
    let path = copy_path(process, path)?;
    let directory = directory
        .resolve::<DirectoryObject>(Rights::READ.union(Rights::INSPECT))
        .map_err(crate::process::ProcessError::from)?;
    Ok(directory.object().metadata(
        &path,
        follow,
        &ScratchBudget::new(&process.resource_domain()),
    )?)
}

pub(crate) fn file_get_metadata(
    file: &InvocationReceiver,
) -> Result<super::Metadata, ServiceError> {
    let file = file
        .resolve::<FileObject>(Rights::INSPECT)
        .map_err(crate::process::ProcessError::from)?;
    Ok(file.object().metadata()?)
}

pub(crate) fn directory_get_self_metadata(
    directory: &InvocationReceiver,
) -> Result<super::Metadata, ServiceError> {
    let directory = directory
        .resolve::<DirectoryObject>(Rights::INSPECT)
        .map_err(crate::process::ProcessError::from)?;
    Ok(directory.object().self_metadata()?)
}

pub(crate) fn directory_set_metadata(
    process: &Process,
    directory: &InvocationReceiver,
    path: UserSlice,
    follow: bool,
    update: super::MetadataUpdate,
) -> Result<(), ServiceError> {
    let path = copy_path(process, path)?;
    let directory = directory
        .resolve::<DirectoryObject>(Rights::READ.union(Rights::SET_ATTRIBUTES))
        .map_err(crate::process::ProcessError::from)?;
    Ok(directory.object().set_metadata(
        &path,
        follow,
        update,
        &ScratchBudget::new(&process.resource_domain()),
    )?)
}

pub(crate) fn file_set_metadata(
    file: &InvocationReceiver,
    update: super::MetadataUpdate,
) -> Result<(), ServiceError> {
    let file = file
        .resolve::<FileObject>(Rights::SET_ATTRIBUTES)
        .map_err(crate::process::ProcessError::from)?;
    Ok(file.object().set_metadata(update)?)
}

pub(crate) fn directory_rename(
    process: &Process,
    source: &InvocationReceiver,
    path: UserSlice,
    destination: HandleValue,
    new_path: UserSlice,
) -> Result<(), ServiceError> {
    let path = copy_path(process, path)?;
    let new_path = copy_path(process, new_path)?;
    let rights = Rights::READ.union(Rights::WRITE);
    let source = source
        .resolve::<DirectoryObject>(rights)
        .map_err(crate::process::ProcessError::from)?;
    let destination = process.resolve_handle::<DirectoryObject>(destination, rights)?;
    Ok(source.object().rename(
        &path,
        destination.object(),
        &new_path,
        &ScratchBudget::new(&process.resource_domain()),
    )?)
}

pub(crate) fn directory_link(
    process: &Process,
    source: &InvocationReceiver,
    path: UserSlice,
    destination: HandleValue,
    new_path: UserSlice,
) -> Result<(), ServiceError> {
    let path = copy_path(process, path)?;
    let new_path = copy_path(process, new_path)?;
    let rights = Rights::READ.union(Rights::WRITE);
    let source = source
        .resolve::<DirectoryObject>(rights)
        .map_err(crate::process::ProcessError::from)?;
    let destination = process.resolve_handle::<DirectoryObject>(destination, rights)?;
    Ok(source.object().link(
        &path,
        destination.object(),
        &new_path,
        &ScratchBudget::new(&process.resource_domain()),
    )?)
}

pub(crate) fn directory_symlink(
    process: &Process,
    directory: &InvocationReceiver,
    path: UserSlice,
    target: UserSlice,
) -> Result<(), ServiceError> {
    let path = copy_path(process, path)?;
    let target = copy_path(process, target)?;
    let directory = directory
        .resolve::<DirectoryObject>(Rights::READ.union(Rights::WRITE))
        .map_err(crate::process::ProcessError::from)?;
    Ok(directory.object().symlink(
        &path,
        &target,
        &ScratchBudget::new(&process.resource_domain()),
    )?)
}

pub(crate) fn directory_read_link(
    process: &Process,
    directory: &InvocationReceiver,
    path: UserSlice,
) -> Result<ScratchVec<u8>, ServiceError> {
    let path = copy_path(process, path)?;
    let directory = directory
        .resolve::<DirectoryObject>(Rights::READ)
        .map_err(crate::process::ProcessError::from)?;
    Ok(directory
        .object()
        .read_link(&path, &ScratchBudget::new(&process.resource_domain()))?)
}

pub(crate) fn directory_canonicalize(
    process: &Process,
    directory: &InvocationReceiver,
    path: UserSlice,
) -> Result<ScratchString, ServiceError> {
    let path = copy_path(process, path)?;
    let directory = directory
        .resolve::<DirectoryObject>(Rights::READ)
        .map_err(crate::process::ProcessError::from)?;
    Ok(directory
        .object()
        .canonicalize(&path, &ScratchBudget::new(&process.resource_domain()))?)
}

pub(crate) fn directory_remove_if(
    process: &Process,
    directory: &InvocationReceiver,
    path: UserSlice,
    is_directory: bool,
    expected: u64,
) -> Result<(), ServiceError> {
    if expected == 0 {
        return Err(ServiceError::InvalidInput);
    }
    let path = copy_path(process, path)?;
    let directory = directory
        .resolve::<DirectoryObject>(Rights::READ.union(Rights::WRITE))
        .map_err(crate::process::ProcessError::from)?;
    Ok(directory.object().remove_if(
        &path,
        is_directory,
        expected,
        &ScratchBudget::new(&process.resource_domain()),
    )?)
}

pub(crate) fn directory_open_directory_nofollow(
    process: &Process,
    directory: &InvocationReceiver,
    path: UserSlice,
    rights: Rights,
) -> Result<HandleValue, ServiceError> {
    let path = copy_path(process, path)?;
    let required = super::rights_contract::directory_rights_for_directory(rights.bits())
        .and_then(Rights::from_bits)
        .ok_or(ServiceError::InvalidInput)?;
    let directory = directory
        .resolve::<DirectoryObject>(required)
        .map_err(crate::process::ProcessError::from)?;
    let child = directory
        .object()
        .open_directory_nofollow(&path, &process.resource_domain())?;
    Ok(process.create_object(child, rights)?)
}

pub(crate) fn file_sync(file: &InvocationReceiver, scope: u64) -> Result<(), ServiceError> {
    if scope > 1 {
        return Err(ServiceError::InvalidInput);
    }
    let file = file
        .resolve::<FileObject>(Rights::WRITE)
        .map_err(crate::process::ProcessError::from)?;
    Ok(file.object().sync(scope)?)
}

pub(crate) fn file_lock(
    process: &Process,
    file: &InvocationReceiver,
    mode: super::locks::LockMode,
    deadline: u64,
    cancelled: impl Fn() -> bool,
) -> Result<(), ServiceError> {
    let file = file
        .resolve::<FileObject>(Rights::LOCK_FILE)
        .map_err(crate::process::ProcessError::from)?;
    file.object()
        .lock(mode, deadline, &process.resource_domain(), cancelled)
        .map_err(ServiceError::FileLock)
}

pub(crate) fn file_unlock(file: &InvocationReceiver) -> Result<(), ServiceError> {
    let file = file
        .resolve::<FileObject>(Rights::LOCK_FILE)
        .map_err(crate::process::ProcessError::from)?;
    file.object().unlock().map_err(ServiceError::FileLock)
}

pub(crate) fn directory_open_file_with_options(
    process: &Process,
    directory: &InvocationReceiver,
    path: UserSlice,
    rights: Rights,
    options: u64,
    mode: u32,
) -> Result<HandleValue, ServiceError> {
    if options & !7 != 0
        || options & 3 == 2
        || mode & !0o777 != 0
        || (options & 4 != 0 && !rights.contains(Rights::WRITE))
    {
        return Err(ServiceError::InvalidInput);
    }
    let path = copy_path(process, path)?;
    let mut required = super::rights_contract::directory_rights_for_file(rights.bits())
        .and_then(Rights::from_bits)
        .ok_or(ServiceError::InvalidInput)?;
    if options & 3 != 0 {
        required = required.union(Rights::WRITE);
    }
    let directory = directory
        .resolve::<DirectoryObject>(required)
        .map_err(crate::process::ProcessError::from)?;
    let options = super::FileOpenOptions::new(rights, options, mode)?;
    prepare_file_handle(process, rights, |prepare| {
        directory.object().open_file_with_options(
            &path,
            &options,
            &process.resource_domain(),
            prepare,
        )
    })
}

/// Reserve table storage and allocate the object before a backend commits I/O.
/// The closure receives preparation, not publication: a disk failure drops an
/// unreachable object, never a handle installed in the requesting process.
fn prepare_file_handle(
    process: &Process,
    rights: Rights,
    operation: impl FnOnce(
        &mut dyn FnMut(FileObject) -> Result<PreparedHandle, ServiceError>,
    ) -> Result<PreparedHandle, ServiceError>,
) -> Result<HandleValue, ServiceError> {
    let reservation = process.reserve_handles::<1>()?;
    let result = operation(&mut |file| {
        let object = ObjectPublication::try_new(file).map_err(ProcessError::from)?;
        PreparedHandle::try_from_new_object(object, rights, HandleFlags::NONE)
            .map_err(ProcessError::from)
            .map_err(Into::into)
    });
    match result {
        Ok(prepared) => process
            .publish_handles(reservation, [prepared])
            .map(|handles| handles[0])
            .map_err(|failure| ServiceError::Process(failure.error)),
        Err(error) => {
            process.abort_handles(reservation);
            Err(error)
        }
    }
}

pub(crate) fn mount_remote(
    process: &Process,
    channel: HandleValue,
    buffer: HandleValue,
    queue: Option<HandleValue>,
    directory: &InvocationReceiver,
    path: UserSlice,
    read_only: bool,
) -> Result<(), ServiceError> {
    let path = copy_path(process, path)?;
    let directory = directory
        .resolve::<DirectoryObject>(Rights::WRITE.union(Rights::EXECUTE))
        .map_err(crate::process::ProcessError::from)?;
    let device = super::remote::Transport::prepare(process, channel, buffer, queue, read_only)
        .map_err(|error| ServiceError::FileSystem(VfsError::Backend(error)))?;
    let publication = device.publication();
    directory
        .object()
        .mount_remote(&path, device, &process.resource_domain())?;
    if let Some(publication) = publication {
        publication.publish();
    }
    Ok(())
}

pub(crate) fn unmount(
    process: &Process,
    directory: &InvocationReceiver,
    path: UserSlice,
) -> Result<(), ServiceError> {
    let path = copy_path(process, path)?;
    let directory = directory
        .resolve::<DirectoryObject>(Rights::WRITE.union(Rights::EXECUTE))
        .map_err(crate::process::ProcessError::from)?;
    directory
        .object()
        .unmount(&path, &process.resource_domain())?;
    Ok(())
}
