// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

#![cfg_attr(
    all(feature = "kernel-test", feature = "kernel-self-test"),
    allow(
        dead_code,
        reason = "Native VFS consumers are replaced by the bare-metal self-test"
    )
)]

//! Kernel-owned VFS namespace, capability objects, and file-data policy.

mod directory;
mod executable;
mod file_data;
pub(crate) mod file_record;
pub(crate) mod instance;
mod lock_state;
pub(crate) mod locks;
mod mount_admission;
mod mounts;
#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
pub(crate) mod namespace_test;
mod objects;
mod protection;
mod protection_state;
mod ramfs;
pub(crate) mod read_ahead;
mod read_contract;
pub(super) mod remote;
mod resolve;
mod resolve_state;
mod rights_contract;
mod root_transition;
mod root_transition_object;
pub(crate) use root_transition_object::RootTransitionObject;
mod scratch;
pub(crate) mod service;

pub(crate) use directory::DirectoryStream;
pub(crate) use executable::ExecutableSnapshot;
pub(crate) use file_record::CachePage;
#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
pub(crate) use objects::NodeLocationInfo;
pub(crate) use objects::{
    DirectoryEntrySnapshot, DirectoryInfo, DirectoryObject, DirectoryPage, Error,
    Error as VfsError, FileInfo, FileObject, FileOpenOptions, Metadata, MetadataUpdate,
};
pub(crate) use protection::{FileProtection, FileProtectionObject};
pub(crate) use root_transition::RootTransition;
pub(crate) use scratch::{ScratchBudget, ScratchString, ScratchVec};
pub(crate) use service::{
    ServiceError as VfsServiceError, begin_directory_read, create_directory, create_file,
    directory_info, file_info, open_directory, open_file, protect_file, read_directory,
    read_file_at, remove_entry, resize_file, write_file_at,
};

use crate::fs::primitives::ramfs::RamFs;
use hyper::mm::{AllocationError, FallibleArc};
use hyper::sync::PublishedOnce;

use instance::{FilesystemInstance, MountNamespace};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum InitializationError {
    Allocation,
    AlreadyInitialized,
    Cache(crate::fs::cache::CacheError),
    FileSystem(instance::Error),
    RamFs(crate::fs::primitives::ramfs::Error),
}

impl From<AllocationError> for InitializationError {
    fn from(_: AllocationError) -> Self {
        Self::Allocation
    }
}

impl From<instance::Error> for InitializationError {
    fn from(error: instance::Error) -> Self {
        Self::FileSystem(error)
    }
}

static SYSTEM_NAMESPACE: PublishedOnce<FallibleArc<MountNamespace>> = PublishedOnce::new();

/// Borrow the system service's cache without creating a namespace or mount pin.
pub(crate) fn file_cache() -> Option<&'static crate::fs::cache::FileDataCache<CachePage>> {
    SYSTEM_NAMESPACE
        .get()
        .map(|namespace| namespace.cache_ref())
}

/// Bounded scheduled cleanup of weak namespace bindings after page reclaim.
/// This is not part of the allocator's emergency callback.
pub(crate) fn reclaim_file_records(limit: usize) -> usize {
    SYSTEM_NAMESPACE
        .get()
        .map_or(0, |namespace| namespace.reclaim_file_records(limit))
}

/// Finite scheduled cleanup after an explicit allocation or quota request.
/// Busy namespace transactions are skipped, never awaited by the worker.
pub(crate) fn reclaim_idle_records(domain: Option<crate::accounting::ResourceDomainId>) {
    if let Some(namespace) = SYSTEM_NAMESPACE.get() {
        namespace.reclaim_idle_records(domain);
    }
}

#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
pub(crate) fn cache_test_record(
    id: u64,
    domain: &crate::accounting::ResourceDomain,
) -> Result<FallibleArc<file_record::FileRecord>, instance::Error> {
    file_record::FileRecord::try_new(id, domain)
}

pub(crate) fn initialize(
    boot: &mut crate::boot::Initialization,
) -> Result<(), InitializationError> {
    let archive = FallibleArc::try_new(
        boot.take_initial_ramdisk()
            .ok_or(InitializationError::AlreadyInitialized)?,
    )?;
    // SAFETY: archive stays owned throughout parsing/construction and every
    // resulting node retains a clone before publication. No archive slice
    // escapes except through a node or parser snapshot carrying that owner.
    let bytes =
        unsafe { core::slice::from_raw_parts(archive.bytes().as_ptr(), archive.bytes().len()) };
    let ramfs = RamFs::from_newc(bytes).map_err(InitializationError::RamFs)?;
    let node_count = ramfs.nodes().len();
    let filesystem = FilesystemInstance::try_from_owned_archive(ramfs, archive)?;
    // The system cache is a VFS-service resource, not namespace-local state.
    // Injecting its owner prevents future namespace creation from silently
    // multiplying the global clean-page budget.
    let cache =
        crate::fs::cache::FileDataCache::try_new_system().map_err(InitializationError::Cache)?;
    let cache = FallibleArc::try_new(cache)?;
    let namespace = MountNamespace::try_new(filesystem, cache)?;
    SYSTEM_NAMESPACE
        .publish(namespace)
        .map_err(|_| InitializationError::AlreadyInitialized)?;
    crate::pr_info!("HypeR: mounted initramfs VFS with {node_count} node(s)");
    Ok(())
}

pub(crate) fn root_directory(
    sponsor: &crate::accounting::ResourceDomain,
) -> Result<DirectoryObject, Error> {
    let namespace = SYSTEM_NAMESPACE.get().ok_or(Error::Missing)?;
    DirectoryObject::try_root(namespace.clone(), sponsor)
}

/// Logical capacities of live writable ramfs payload reservations. These bytes
/// are backed by the heap; archive data and directory metadata are excluded.
pub(crate) fn ramfs_capacity_bytes() -> u64 {
    ramfs::capacity_bytes()
}

mod write_source;
