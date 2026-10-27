// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Capability objects for namespace traversal and file access.

use hyper::mm::FallibleArc;
use {
    crate::fs::primitives::MAX_NAME_BYTES, crate::fs::primitives::Name,
    crate::fs::primitives::NodeAttributes, crate::fs::primitives::NodeKind,
};

use crate::authority::Rights;
use crate::fs::cache::FileDataCache;
use {
    crate::accounting::CommittedCharge, crate::accounting::ResourceAmount,
    crate::accounting::ResourceDomain, crate::accounting::ResourceError,
    crate::accounting::ResourceKind,
};
use {
    crate::object::KernelObject, crate::object::ObjectCreationError, crate::object::ObjectKind,
    crate::object::TransferClass, crate::object::object_allocation_size, crate::object::private,
};

use super::file_record::CachePage;
use super::instance::{Creation, DirectoryAnchor, EntryName, Location, MountNamespace};

#[path = "directory_namespace.rs"]
mod namespace;
use super::scratch::{ScratchBudget, ScratchString, ScratchVec};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Error {
    Allocation,
    AllocationSize,
    Backend(super::instance::Error),
    InvalidDirectoryCookie,
    InvalidPath,
    Missing,
    AlreadyExists,
    NotEmpty,
    InvalidSize,
    NotDirectory,
    NotExecutable,
    NotRegularFile,
    IsDirectory,
    SymlinkLoop,
    NotSymlink,
    InvalidInput,
    AccessDenied,
    Busy,
    Unsupported,
    CrossDevice,
    Object(ObjectCreationError),
    Resource(ResourceError),
}

impl From<ObjectCreationError> for Error {
    fn from(error: ObjectCreationError) -> Self {
        Self::Object(error)
    }
}

impl From<ResourceError> for Error {
    fn from(error: ResourceError) -> Self {
        Self::Resource(error)
    }
}

impl From<super::instance::Error> for Error {
    fn from(error: super::instance::Error) -> Self {
        match error {
            super::instance::Error::InvalidDirectoryCookie => Self::InvalidDirectoryCookie,
            super::instance::Error::AlreadyExists => Self::AlreadyExists,
            super::instance::Error::Missing => Self::Missing,
            super::instance::Error::NotEmpty => Self::NotEmpty,
            super::instance::Error::InvalidSize => Self::InvalidSize,
            super::instance::Error::Resource(error) => Self::Resource(error),
            super::instance::Error::Allocation => Self::Allocation,
            super::instance::Error::NotDirectory => Self::NotDirectory,
            super::instance::Error::NotRegularFile => Self::NotRegularFile,
            super::instance::Error::IsDirectory => Self::IsDirectory,
            super::instance::Error::NotSymlink => Self::NotSymlink,
            super::instance::Error::InvalidInput => Self::InvalidInput,
            super::instance::Error::Busy => Self::Busy,
            super::instance::Error::Unsupported => Self::Unsupported,
            other => Self::Backend(other),
        }
    }
}

pub(crate) const DIRECTORY_PAGE_CAPACITY: usize =
    hyper::abi::native::HYPER_NATIVE_DIRECTORY_ENTRY_PAGE_CAPACITY as usize;
const _: () = assert!(
    MAX_NAME_BYTES == hyper::abi::native::HYPER_NATIVE_DIRECTORY_ENTRY_NAME_MAX_BYTES as usize
);

#[derive(Clone, Copy)]
pub(crate) struct DirectoryEntrySnapshot {
    pub(crate) name: [u8; MAX_NAME_BYTES],
    pub(crate) name_length: u32,
    pub(crate) attributes: NodeAttributes,
}

pub(crate) struct DirectoryPage {
    pub(super) entries: [Option<DirectoryEntrySnapshot>; DIRECTORY_PAGE_CAPACITY],
    pub(super) len: usize,
    pub(super) next_cookie: u64,
}

impl DirectoryPage {
    pub(crate) const fn empty() -> Self {
        Self {
            entries: [None; DIRECTORY_PAGE_CAPACITY],
            len: 0,
            next_cookie: 0,
        }
    }

    pub(crate) fn entries(&self) -> impl Iterator<Item = &DirectoryEntrySnapshot> {
        self.entries[..self.len].iter().filter_map(Option::as_ref)
    }

    pub(crate) const fn len(&self) -> usize {
        self.len
    }

    pub(crate) const fn next_cookie(&self) -> u64 {
        self.next_cookie
    }
}

/// Authority to resolve paths below one namespace location.
pub(crate) struct DirectoryObject {
    namespace: FallibleArc<MountNamespace>,
    root: DirectoryAnchor,
    current: DirectoryAnchor,
    _object_charge: CommittedCharge,
}

/// Observation-only identity for one node at one namespace location.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct NodeLocationInfo {
    pub(crate) filesystem_id: u64,
    pub(crate) mount_id: u64,
    pub(crate) node_id: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct FileInfo {
    pub(crate) location: NodeLocationInfo,
    pub(crate) size: u64,
    pub(crate) mode: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DirectoryInfo {
    pub(crate) location: NodeLocationInfo,
    pub(crate) mode: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Metadata {
    pub(crate) location: NodeLocationInfo,
    pub(crate) attributes: NodeAttributes,
    pub(crate) accessed: Option<crate::time::queue::Timestamp>,
    pub(crate) modified: Option<crate::time::queue::Timestamp>,
    pub(crate) created: Option<crate::time::queue::Timestamp>,
    pub(crate) changed: Option<crate::time::queue::Timestamp>,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct MetadataUpdate {
    pub(crate) mode: Option<u32>,
    pub(crate) accessed: Option<crate::time::queue::Timestamp>,
    pub(crate) modified: Option<crate::time::queue::Timestamp>,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum FileCreation {
    Existing,
    Create,
    CreateNew,
}
#[derive(Clone, Copy)]
pub(crate) struct FileOpenOptions {
    rights: Rights,
    creation: FileCreation,
    truncate: bool,
    mode: u32,
}
impl FileOpenOptions {
    pub(crate) fn new(rights: Rights, bits: u64, mode: u32) -> Result<Self, Error> {
        if bits & !7 != 0
            || bits & 3 == 3
            || mode & !0o777 != 0
            || (bits != 0 && !rights.contains(Rights::WRITE))
        {
            return Err(Error::InvalidInput);
        }
        let creation = match bits & 3 {
            0 => FileCreation::Existing,
            1 => FileCreation::Create,
            _ => FileCreation::CreateNew,
        };
        Ok(Self {
            rights,
            creation,
            truncate: bits & 4 != 0,
            mode,
        })
    }
}

fn metadata(location: &Location) -> Result<Metadata, Error> {
    let (attributes, times) = location.mount().filesystem().metadata(location.node())?;
    Ok(Metadata {
        location: location_info(location),
        attributes,
        accessed: times.accessed,
        modified: times.modified,
        created: times.created,
        changed: times.changed,
    })
}

impl DirectoryObject {
    pub(crate) fn try_root(
        namespace: FallibleArc<MountNamespace>,
        sponsor: &ResourceDomain,
    ) -> Result<Self, Error> {
        Ok(Self {
            namespace,
            root: DirectoryAnchor::NamespaceRoot,
            current: DirectoryAnchor::NamespaceRoot,
            _object_charge: reserve_object_charge::<Self>(sponsor)?,
        })
    }

    pub(crate) fn open_file(
        &self,
        path: &str,
        sponsor: &ResourceDomain,
    ) -> Result<FileObject, Error> {
        let location = super::resolve::typed(
            &self.namespace,
            &self.root,
            &self.current,
            path,
            NodeKind::File,
            true,
            &ScratchBudget::new(sponsor),
        )?;
        FileObject::try_new(location, self.namespace.cache(), sponsor)
    }

    pub(crate) fn open_directory(
        &self,
        path: &str,
        sponsor: &ResourceDomain,
    ) -> Result<Self, Error> {
        let location = super::resolve::typed(
            &self.namespace,
            &self.root,
            &self.current,
            path,
            NodeKind::Directory,
            true,
            &ScratchBudget::new(sponsor),
        )?;
        Self::try_new(self.namespace.clone(), location, sponsor)
    }

    pub(crate) fn create_file<R, E: From<Error> + From<super::instance::Error>>(
        &self,
        path: &str,
        mode: u32,
        sponsor: &ResourceDomain,
        publish: impl FnOnce(FileObject) -> Result<R, E>,
    ) -> Result<R, E> {
        let scratch = &ScratchBudget::new(sponsor);
        let (parent, name, epoch, directory_required) = self.parent(path, scratch)?;
        parent.mount().filesystem().create_at(
            parent.node(),
            EntryName {
                name: Name::new(&name).map_err(|_| Error::InvalidPath)?,
                directory_required,
            },
            Creation {
                kind: NodeKind::File,
                mode,
                target: None,
            },
            epoch,
            |node, attributes| {
                let location = Location::new(parent.mount().clone(), node)?;
                publish(FileObject::try_new_admitted(
                    location,
                    self.namespace.cache(),
                    sponsor,
                    attributes,
                    true,
                )?)
            },
        )
    }
    pub(crate) fn create(
        &self,
        path: &str,
        kind: NodeKind,
        mode: u32,
        scratch: &ScratchBudget,
    ) -> Result<(), Error> {
        let (parent, name, epoch, directory_required) = self.parent(path, scratch)?;
        parent.mount().filesystem().create_at(
            parent.node(),
            EntryName {
                name: Name::new(&name).map_err(|_| Error::InvalidPath)?,
                directory_required,
            },
            Creation {
                kind,
                mode,
                target: None,
            },
            epoch,
            |_, _| Ok(()),
        )
    }
    pub(crate) fn remove(
        &self,
        path: &str,
        kind: NodeKind,
        scratch: &ScratchBudget,
    ) -> Result<(), Error> {
        self.remove_expected(path, kind, None, scratch)
    }
    pub(crate) fn remove_if(
        &self,
        path: &str,
        is_directory: bool,
        expected: u64,
        scratch: &ScratchBudget,
    ) -> Result<(), Error> {
        self.remove_expected(
            path,
            if is_directory {
                NodeKind::Directory
            } else {
                NodeKind::File
            },
            Some(expected),
            scratch,
        )
    }
    fn remove_expected(
        &self,
        path: &str,
        kind: NodeKind,
        expected: Option<u64>,
        scratch: &ScratchBudget,
    ) -> Result<(), Error> {
        let (parent, name, epoch, directory_required) = self.parent(path, scratch)?;
        parent
            .mount()
            .filesystem()
            .remove_at(
                parent.node(),
                EntryName {
                    name: Name::new(&name).map_err(|_| Error::InvalidPath)?,
                    directory_required,
                },
                kind,
                expected,
                epoch,
            )
            .map_err(Into::into)
    }
    fn parent(
        &self,
        path: &str,
        scratch: &ScratchBudget,
    ) -> Result<(Location, ScratchString, u64, bool), Error> {
        if path.is_empty()
            || path.len() > crate::fs::primitives::MAX_PATH_BYTES
            || path.as_bytes().contains(&0)
        {
            return Err(Error::InvalidPath);
        }
        let directory_required = path.ends_with('/');
        let path = path.trim_end_matches('/');
        let (prefix, name) = path.rsplit_once('/').unwrap_or((".", path));
        let name = Name::new(name).map_err(|_| Error::InvalidPath)?;
        let resolved = self.resolve(
            if prefix.is_empty() { "/" } else { prefix },
            true,
            false,
            scratch,
        )?;
        let parent = resolved.existing()?.clone();
        if parent
            .mount()
            .filesystem()
            .attributes(parent.node())?
            .kind()
            != NodeKind::Directory
        {
            return Err(Error::NotDirectory);
        }
        let owned = ScratchString::from_str(name.as_str(), scratch.clone()).map_err(Error::from)?;
        Ok((parent, owned, resolved.epoch, directory_required))
    }
    fn resolve(
        &self,
        path: &str,
        follow: bool,
        missing: bool,
        scratch: &ScratchBudget,
    ) -> Result<super::resolve::Resolved, Error> {
        super::resolve::resolve(
            &self.namespace,
            &self.root,
            &self.current,
            path,
            follow,
            missing,
            scratch,
        )
    }
    pub(crate) fn scope(
        root: &Self,
        start: &Self,
        sponsor: &ResourceDomain,
    ) -> Result<Self, Error> {
        if !core::ptr::eq(&*root.namespace, &*start.namespace) {
            return Err(Error::CrossDevice);
        }
        let resolved = super::resolve::resolve(
            &root.namespace,
            &root.current,
            &start.current,
            ".",
            true,
            false,
            &ScratchBudget::new(sponsor),
        )?;
        Ok(Self {
            namespace: root.namespace.clone(),
            root: root.current.clone(),
            current: if root.current.follows_root() {
                start.current.clone()
            } else {
                DirectoryAnchor::Fixed(resolved.existing()?.clone())
            },
            _object_charge: reserve_object_charge::<Self>(sponsor)?,
        })
    }
    pub(crate) fn open_directory_nofollow(
        &self,
        path: &str,
        sponsor: &ResourceDomain,
    ) -> Result<Self, Error> {
        let location = super::resolve::typed(
            &self.namespace,
            &self.root,
            &self.current,
            path,
            NodeKind::Directory,
            false,
            &ScratchBudget::new(sponsor),
        )?;
        Self::try_new(self.namespace.clone(), location, sponsor)
    }
    pub(crate) fn metadata(
        &self,
        path: &str,
        follow: bool,
        scratch: &ScratchBudget,
    ) -> Result<Metadata, Error> {
        metadata(self.resolve(path, follow, false, scratch)?.existing()?)
    }
    pub(crate) fn self_metadata(&self) -> Result<Metadata, Error> {
        metadata(&self.current.location(&self.namespace.mounts.snapshot()))
    }
    pub(crate) fn set_metadata(
        &self,
        path: &str,
        follow: bool,
        update: MetadataUpdate,
        scratch: &ScratchBudget,
    ) -> Result<(), Error> {
        let resolved = self.resolve(path, follow, false, scratch)?;
        let location = resolved.existing()?;
        location
            .mount()
            .filesystem()
            .set_metadata(location.node(), update)
            .map_err(Into::into)
    }
    pub(crate) fn canonicalize(
        &self,
        path: &str,
        scratch: &ScratchBudget,
    ) -> Result<ScratchString, Error> {
        Ok(self.resolve(path, true, false, scratch)?.canonical)
    }
    pub(crate) fn read_link(
        &self,
        path: &str,
        scratch: &ScratchBudget,
    ) -> Result<ScratchVec<u8>, Error> {
        let resolved = self.resolve(path, false, false, scratch)?;
        let location = resolved.existing()?;
        let attributes = location.mount().filesystem().attributes(location.node())?;
        if attributes.kind() != NodeKind::Symlink {
            return Err(Error::NotSymlink);
        }
        super::resolve::read_link(location, attributes.size(), scratch)
    }
    pub(crate) fn symlink(
        &self,
        path: &str,
        target: &str,
        scratch: &ScratchBudget,
    ) -> Result<(), Error> {
        if target.is_empty()
            || target.len() > crate::fs::primitives::MAX_PATH_BYTES
            || target.as_bytes().contains(&0)
        {
            return Err(Error::InvalidPath);
        }
        let (parent, name, epoch, directory_required) = self.parent(path, scratch)?;
        parent.mount().filesystem().create_at(
            parent.node(),
            EntryName {
                name: Name::new(&name).map_err(|_| Error::InvalidPath)?,
                directory_required,
            },
            Creation {
                kind: NodeKind::Symlink,
                mode: 0o777,
                target: Some(target.as_bytes()),
            },
            epoch,
            |_, _| Ok(()),
        )
    }
    pub(crate) fn rename(
        &self,
        path: &str,
        destination: &Self,
        new_path: &str,
        scratch: &ScratchBudget,
    ) -> Result<(), Error> {
        let (source, name, epoch, directory_required) = self.parent(path, scratch)?;
        let (target, new_name, target_epoch, target_directory_required) =
            destination.parent(new_path, scratch)?;
        if source.mount().id() != target.mount().id()
            || !core::ptr::eq(&*self.namespace, &*destination.namespace)
        {
            return Err(Error::CrossDevice);
        }
        if epoch != target_epoch {
            return Err(Error::Busy);
        }
        source
            .mount()
            .filesystem()
            .rename(
                source.node(),
                EntryName {
                    name: Name::new(&name).map_err(|_| Error::InvalidPath)?,
                    directory_required,
                },
                target.node(),
                EntryName {
                    name: Name::new(&new_name).map_err(|_| Error::InvalidPath)?,
                    directory_required: target_directory_required,
                },
                epoch,
            )
            .map_err(Into::into)
    }
    pub(crate) fn link(
        &self,
        path: &str,
        destination: &Self,
        new_path: &str,
        scratch: &ScratchBudget,
    ) -> Result<(), Error> {
        let source = self.resolve(path, false, false, scratch)?;
        let node = source.existing()?;
        let (target, new_name, epoch, target_directory_required) =
            destination.parent(new_path, scratch)?;
        if node.mount().id() != target.mount().id()
            || !core::ptr::eq(&*self.namespace, &*destination.namespace)
        {
            return Err(Error::CrossDevice);
        }
        if source.epoch != epoch {
            return Err(Error::Busy);
        }
        node.mount()
            .filesystem()
            .link(
                node.node(),
                target.node(),
                EntryName {
                    name: Name::new(&new_name).map_err(|_| Error::InvalidPath)?,
                    directory_required: target_directory_required,
                },
                epoch,
            )
            .map_err(Into::into)
    }
    pub(crate) fn open_file_with_options<R, E: From<Error> + From<super::instance::Error>>(
        &self,
        path: &str,
        options: &FileOpenOptions,
        sponsor: &ResourceDomain,
        publish: impl FnOnce(FileObject) -> Result<R, E>,
    ) -> Result<R, E> {
        let FileOpenOptions { creation, mode, .. } = *options;
        if creation == FileCreation::CreateNew {
            return self.create_file(path, mode, sponsor, publish);
        }
        let resolved = self.resolve(
            path,
            true,
            creation == FileCreation::Create,
            &ScratchBudget::new(sponsor),
        )?;
        self.open_resolved_file(&resolved, options, sponsor, publish)
    }

    // Resolution temporaries do not belong to the backend commit phase. Keep
    // the admitted path result borrowed while preparation/truncation may block.
    #[inline(never)]
    fn open_resolved_file<R, E: From<Error> + From<super::instance::Error>>(
        &self,
        resolved: &super::resolve::Resolved,
        options: &FileOpenOptions,
        sponsor: &ResourceDomain,
        publish: impl FnOnce(FileObject) -> Result<R, E>,
    ) -> Result<R, E> {
        let FileOpenOptions {
            rights,
            truncate,
            mode,
            ..
        } = *options;
        if let Some(location) = &resolved.location {
            location
                .mount()
                .filesystem()
                .open_existing(location.node(), truncate, |attributes| {
                    let file = FileObject::try_new_admitted(
                        location.clone(),
                        self.namespace.cache(),
                        sponsor,
                        attributes,
                        false,
                    )?;
                    file.validate_access(rights)?;
                    publish(file)
                })
        } else {
            let parent = &resolved.parent;
            parent.mount().filesystem().create_at(
                parent.node(),
                EntryName {
                    name: Name::new(&resolved.name).map_err(|_| Error::InvalidPath)?,
                    directory_required: false,
                },
                Creation {
                    kind: NodeKind::File,
                    mode,
                    target: None,
                },
                resolved.epoch,
                |node, attributes| {
                    publish(FileObject::try_new_admitted(
                        Location::new(parent.mount().clone(), node)?,
                        self.namespace.cache(),
                        sponsor,
                        attributes,
                        true,
                    )?)
                },
            )
        }
    }

    pub(crate) fn begin_read(
        &self,
        sponsor: &ResourceDomain,
    ) -> Result<super::DirectoryStream, Error> {
        let location = self.current.location(&self.namespace.mounts.snapshot());
        super::DirectoryStream::try_new(self.namespace.clone(), location, sponsor)
    }

    pub(crate) fn info(&self) -> Result<DirectoryInfo, Error> {
        let current = self.current.location(&self.namespace.mounts.snapshot());
        Ok(DirectoryInfo {
            location: location_info(&current),
            mode: metadata(&current)?.attributes.mode(),
        })
    }

    fn try_new(
        namespace: FallibleArc<MountNamespace>,
        root: Location,
        sponsor: &ResourceDomain,
    ) -> Result<Self, Error> {
        let attributes = root
            .mount()
            .filesystem()
            .attributes(root.node())
            .map_err(Error::from)?;
        if attributes.kind() != NodeKind::Directory {
            return Err(Error::NotDirectory);
        }
        Ok(Self {
            namespace,
            current: DirectoryAnchor::Fixed(root.clone()),
            root: DirectoryAnchor::Fixed(root),
            _object_charge: reserve_object_charge::<Self>(sponsor)?,
        })
    }
}

impl private::Sealed for DirectoryObject {}
impl private::UserExportable for DirectoryObject {}

impl KernelObject for DirectoryObject {
    const KIND: ObjectKind = ObjectKind::DIRECTORY;
    const SUPPORTED_RIGHTS: Rights = Rights::DUPLICATE
        .union(Rights::TRANSFER)
        .union(Rights::INSPECT)
        .union(Rights::READ)
        .union(Rights::WRITE)
        .union(Rights::EXECUTE)
        .union(Rights::SET_ATTRIBUTES)
        .union(Rights::LOCK_FILE)
        .union(Rights::PROTECT)
        .union(Rights::MANAGE_NAMESPACE);
    const TRANSFER_CLASS: TransferClass = TransferClass::Leaf;
}

/// Immutable authority to one opened regular file.
pub(crate) struct FileObject {
    location: Location,
    admitted_rights: Rights,
    lock_owner: super::locks::LockOwner,
    cache: FallibleArc<FileDataCache<CachePage>>,
    read_ahead: hyper::sync::InterruptSpinLock<
        crate::fs::cache::read_ahead::SequentialReader,
        crate::hal::irq::LocalMask,
    >,
    _object_charge: CommittedCharge,
}

impl FileObject {
    /// Acquire content stability independently of this open handle's lifetime.
    /// The caller must check PROTECT authority before publishing the guard.
    pub(crate) fn protect(&self, sponsor: &ResourceDomain) -> Result<super::FileProtection, Error> {
        super::FileProtection::acquire(self.location.clone(), self.cache.clone(), sponsor)
    }

    pub(super) fn location(&self) -> &Location {
        &self.location
    }

    fn try_new(
        location: Location,
        cache: FallibleArc<FileDataCache<CachePage>>,
        sponsor: &ResourceDomain,
    ) -> Result<Self, Error> {
        let attributes = location
            .mount()
            .filesystem()
            .attributes(location.node())
            .map_err(Error::from)?;
        if attributes.kind() != NodeKind::File {
            return Err(if attributes.kind() == NodeKind::Directory {
                Error::IsDirectory
            } else {
                Error::NotRegularFile
            });
        }
        Self::try_new_admitted(location, cache, sponsor, attributes, false)
    }
    fn try_new_admitted(
        location: Location,
        cache: FallibleArc<FileDataCache<CachePage>>,
        sponsor: &ResourceDomain,
        attributes: NodeAttributes,
        creator: bool,
    ) -> Result<Self, Error> {
        if attributes.kind() != NodeKind::File {
            return Err(if attributes.kind() == NodeKind::Directory {
                Error::IsDirectory
            } else {
                Error::NotRegularFile
            });
        }
        let mut bits = super::rights_contract::FILE_SUPPORTED_RIGHTS;
        if !creator {
            if attributes.mode() & 0o444 == 0 {
                bits &= !Rights::READ.bits();
            }
            if attributes.mode() & 0o222 == 0 {
                bits &= !Rights::WRITE.bits();
            }
            if attributes.mode() & 0o111 == 0 {
                bits &= !Rights::EXECUTE.bits();
            }
        }
        let admitted_rights = Rights::from_bits(bits).ok_or(Error::InvalidInput)?;
        let lock_owner =
            location
                .node()
                .locks()
                .create_owner(sponsor)
                .map_err(|error| match error {
                    super::locks::LockOwnerError::Allocation => Error::Allocation,
                    super::locks::LockOwnerError::Resource(error) => Error::Resource(error),
                    super::locks::LockOwnerError::IdentifierExhausted => {
                        Error::Backend(super::instance::Error::IdentifierExhausted)
                    }
                })?;
        Ok(Self {
            location,
            admitted_rights,
            lock_owner,
            cache,
            read_ahead: hyper::sync::InterruptSpinLock::new(
                crate::fs::cache::read_ahead::SequentialReader::new(),
            ),
            _object_charge: reserve_object_charge::<Self>(sponsor)?,
        })
    }
    pub(crate) fn validate_access(&self, rights: Rights) -> Result<(), Error> {
        if self.admitted_rights.contains(rights) {
            Ok(())
        } else {
            Err(Error::AccessDenied)
        }
    }
    pub(crate) fn metadata(&self) -> Result<Metadata, Error> {
        metadata(&self.location)
    }
    pub(crate) fn set_metadata(&self, update: MetadataUpdate) -> Result<(), Error> {
        self.location
            .mount()
            .filesystem()
            .set_metadata(self.location.node(), update)
            .map_err(Into::into)
    }
    pub(crate) fn sync(&self, scope: u64) -> Result<(), Error> {
        self.location
            .mount()
            .filesystem()
            .sync(self.location.node(), scope)
            .map_err(Into::into)
    }
    pub(crate) fn lock(
        &self,
        mode: super::locks::LockMode,
        deadline: u64,
        sponsor: &ResourceDomain,
        cancelled: impl Fn() -> bool,
    ) -> Result<(), super::locks::LockError> {
        self.location
            .node()
            .locks()
            .lock(&self.lock_owner, mode, deadline, sponsor, cancelled)
    }
    pub(crate) fn unlock(&self) -> Result<(), super::locks::LockError> {
        self.location.node().locks().unlock(&self.lock_owner)
    }

    pub(crate) fn len(&self) -> Result<u64, Error> {
        self.location
            .mount()
            .filesystem()
            .file_len(self.location.node())
            .map_err(Into::into)
    }

    pub(crate) fn info(&self) -> Result<FileInfo, Error> {
        Ok(FileInfo {
            location: location_info(&self.location),
            size: self.len()?,
            mode: self.metadata()?.attributes.mode(),
        })
    }

    pub(crate) fn write(&self, offset: Option<u64>, input: &[u8]) -> Result<(usize, u64), Error> {
        self.location
            .mount()
            .filesystem()
            .write_at(self.location.node(), offset, input)
            .map_err(Into::into)
    }

    pub(crate) fn resize(&self, length: u64) -> Result<(), Error> {
        self.location
            .mount()
            .filesystem()
            .resize(self.location.node(), length)
            .map_err(Into::into)
    }

    pub(crate) fn read(&self, offset: u64, destination: &mut [u8]) -> Result<usize, Error> {
        let filesystem = self.location.mount().filesystem_owner();
        let (bytes, observation) = filesystem
            .read_file_observed(self.location.node(), &self.cache, offset, destination)
            .map_err(Error::from)?;
        if let Some(observation) = observation {
            let range = self.read_ahead.with(|reader| {
                reader.observe(
                    observation.identity,
                    observation.length,
                    offset,
                    bytes,
                    observation.had_miss,
                )
            });
            if let Some(range) = range {
                super::read_ahead::submit(
                    filesystem,
                    self.location.node(),
                    &self.cache,
                    observation.identity,
                    range,
                );
            }
        }
        Ok(bytes)
    }

    pub(crate) fn executable_snapshot(
        &self,
        sponsor: &ResourceDomain,
    ) -> Result<super::ExecutableSnapshot, Error> {
        self.location
            .mount()
            .filesystem()
            .executable_snapshot(self.location.node(), &self.cache, sponsor)
            .map_err(Error::from)?
            .ok_or(Error::NotExecutable)
    }
}

impl private::Sealed for FileObject {}
impl private::UserExportable for FileObject {}

impl KernelObject for FileObject {
    const KIND: ObjectKind = ObjectKind::FILE;
    const SUPPORTED_RIGHTS: Rights = Rights::DUPLICATE
        .union(Rights::TRANSFER)
        .union(Rights::INSPECT)
        .union(Rights::READ)
        .union(Rights::WRITE)
        .union(Rights::EXECUTE)
        .union(Rights::SET_ATTRIBUTES)
        .union(Rights::LOCK_FILE)
        .union(Rights::PROTECT);
    const TRANSFER_CLASS: TransferClass = TransferClass::Leaf;

    fn supported_rights(&self) -> Rights {
        self.admitted_rights
    }
    fn on_zero_active_handles(&self, _: &mut crate::object::ObjectRetirement) {
        self.location.node().locks().close(&self.lock_owner);
    }
}

fn location_info(location: &Location) -> NodeLocationInfo {
    NodeLocationInfo {
        filesystem_id: location.mount().filesystem().id().get(),
        mount_id: location.mount().id().get(),
        node_id: location.node().get(),
    }
}

fn reserve_object_charge<T: KernelObject>(
    domain: &ResourceDomain,
) -> Result<CommittedCharge, Error> {
    let bytes = object_allocation_size::<T>()
        .and_then(|value| u64::try_from(value).ok())
        .ok_or(Error::AllocationSize)?;
    Ok(domain
        .reserve(
            ResourceAmount::ZERO
                .with(ResourceKind::KernelObjects, 1)
                .with(ResourceKind::KernelMemoryBytes, bytes),
        )?
        .commit())
}
