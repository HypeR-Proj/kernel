// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Kernel-resident storage with stable node leases and transactional names.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use crate::fs::primitives::ramfs::RamFs;
use crate::time::queue::Timestamp;
use hyper::mm::{FallibleArc, WeakFallibleArc};
use hyper::sync::SpinLock;
use {
    crate::fs::primitives::MAX_NAME_BYTES, crate::fs::primitives::MAX_PATH_BYTES,
    crate::fs::primitives::MAX_PATH_COMPONENTS, crate::fs::primitives::Name,
    crate::fs::primitives::NodeAttributes, crate::fs::primitives::NodeKind,
};
use {crate::fs::primitives::file_data::FileData, crate::fs::primitives::file_data::StorageBudget};

use super::instance::{Creation, EntryName, EntrySnapshot, Error, NodeMetadata};
use super::scratch::{ScratchBudget, ScratchString, ScratchVec};
use super::{ExecutableSnapshot, MetadataUpdate};
use {
    crate::accounting::CommittedCharge, crate::accounting::ResourceAmount,
    crate::accounting::ResourceDomain, crate::accounting::ResourceError,
    crate::accounting::ResourceKind, crate::accounting::ResourceLimits,
};
use {crate::sync::Mutex, crate::sync::MutexGuard};

pub(super) mod promotion;

const STORAGE_LIMIT: u64 = 256 * 1024 * 1024;

pub(super) struct Budget(ResourceDomain);
impl StorageBudget for Budget {
    type Charge = CommittedCharge;
    type Error = ResourceError;
    fn reserve(&self, bytes: usize) -> Result<CommittedCharge, ResourceError> {
        self.0
            .reserve(ResourceAmount::ZERO.with(ResourceKind::KernelMemoryBytes, bytes as u64))
            .map(|charge| charge.commit())
    }
}

static DATA_CAPACITY_BYTES: AtomicU64 = AtomicU64::new(0);

pub(super) fn capacity_bytes() -> u64 {
    DATA_CAPACITY_BYTES.load(Ordering::Relaxed)
}

struct DataBudget<'a>(&'a Budget);
struct DataCharge {
    _charge: CommittedCharge,
    bytes: u64,
}
impl StorageBudget for DataBudget<'_> {
    type Charge = DataCharge;
    type Error = ResourceError;

    fn reserve(&self, bytes: usize) -> Result<DataCharge, ResourceError> {
        let charge = self.0.reserve(bytes)?;
        // This is a diagnostic sum of live payload-capacity reservations, not
        // allocator backing. Failed allocations and replaced buffers release
        // their reservation through the same owner.
        DATA_CAPACITY_BYTES.fetch_add(bytes as u64, Ordering::Relaxed);
        Ok(DataCharge {
            _charge: charge,
            bytes: bytes as u64,
        })
    }
}
impl Drop for DataCharge {
    fn drop(&mut self) {
        DATA_CAPACITY_BYTES.fetch_sub(self.bytes, Ordering::Relaxed);
    }
}

pub(super) struct Ramfs {
    root: FallibleArc<Node>,
    next_id: AtomicU64,
    budget: Budget,
    mutation: Mutex<()>,
    epoch: AtomicU64,
}

/// Only directory names retain strong downward edges. Parent hints are weak;
/// a caller must validate the corresponding forward edge before using one.
struct Topology {
    links: u64,
    parent: Option<WeakFallibleArc<Node>>,
}

pub(super) struct Node {
    id: u64,
    mount_pins: AtomicU64,
    mount_roots: AtomicU64,
    kind: NodeKind,
    topology: SpinLock<Topology>,
    metadata: SpinLock<NodeMetadata>,
    data: Mutex<NodeData>,
    children: Mutex<Children>,
    pub(super) locks: super::locks::FileLocks,
    pub(super) content: super::file_data::FileContent,
    // Per-node: root promotion can move an archive subtree to another mount,
    // and open or unlinked files may outlive the original archive root.
    archive: Option<FallibleArc<crate::boot::Archive>>,
    _charge: CommittedCharge,
}

struct NodeData {
    contents: FileData<'static, DataCharge>,
}
impl NodeData {
    const fn new(contents: FileData<'static, DataCharge>) -> Self {
        Self { contents }
    }
}

struct Children {
    entries: Vec<Entry>,
    next_cookie: u64,
    _charge: Option<CommittedCharge>,
}
struct Entry {
    name: [u8; MAX_NAME_BYTES],
    length: usize,
    cookie: u64,
    node: FallibleArc<Node>,
}

/// Odd epochs describe an in-progress namespace edit. The sleeping mutation
/// lock serializes writers; readers validate one even epoch after traversal.
pub(super) struct Mutation<'a> {
    _guard: MutexGuard<'a, ()>,
    epoch: &'a AtomicU64,
    committed_epoch: u64,
}
impl Drop for Mutation<'_> {
    fn drop(&mut self) {
        self.epoch.store(self.committed_epoch, Ordering::Release);
    }
}

impl Children {
    const fn new() -> Self {
        Self {
            entries: Vec::new(),
            next_cookie: 1,
            _charge: None,
        }
    }
    fn prepare_insert(&mut self, budget: &Budget) -> Result<u64, Error> {
        let cookie = self.next_cookie;
        cookie.checked_add(1).ok_or(Error::IdentifierExhausted)?;
        if self.entries.len() == self.entries.capacity() {
            let capacity = self
                .entries
                .len()
                .checked_add(1)
                .and_then(usize::checked_next_power_of_two)
                .ok_or(Error::Allocation)?;
            let bytes = capacity
                .checked_mul(core::mem::size_of::<Entry>())
                .ok_or(Error::Allocation)?;
            let charge = budget.reserve(bytes).map_err(Error::Resource)?;
            let mut entries = Vec::new();
            entries
                .try_reserve_exact(capacity)
                .map_err(|_| Error::Allocation)?;
            entries.append(&mut self.entries);
            self.entries = entries;
            self._charge = Some(charge);
        }
        Ok(cookie)
    }
    fn insert(&mut self, name: Name<'_>, node: FallibleArc<Node>, cookie: u64) {
        let mut bytes = [0; MAX_NAME_BYTES];
        bytes[..name.as_bytes().len()].copy_from_slice(name.as_bytes());
        self.entries.push(Entry {
            name: bytes,
            length: name.as_bytes().len(),
            cookie,
            node,
        });
        self.next_cookie = cookie + 1;
    }
    fn index(&self, name: Name<'_>) -> Option<usize> {
        self.entries
            .iter()
            .position(|entry| &entry.name[..entry.length] == name.as_bytes())
    }
}

pub(super) struct MountPin(FallibleArc<Node>);
impl Drop for MountPin {
    fn drop(&mut self) {
        if self.0.mount_pins.fetch_sub(1, Ordering::AcqRel) == 0 {
            crate::diagnostics::crash::fatal(format_args!("VFS mount pin underflow"));
        }
    }
}

/// A detached directory stays writable while a mount owns it as its root.
/// This is distinct from pins on covered mountpoints and directory links.
pub(super) struct RootOwner(FallibleArc<Node>);

impl RootOwner {
    pub(super) fn acquire(node: &FallibleArc<Node>) -> Result<Self, Error> {
        node.require(NodeKind::Directory)?;
        node.mount_roots
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                count.checked_add(1)
            })
            .map_err(|_| Error::IdentifierExhausted)?;
        Ok(Self(node.clone()))
    }
}

impl Drop for RootOwner {
    fn drop(&mut self) {
        if self.0.mount_roots.fetch_sub(1, Ordering::AcqRel) == 0 {
            crate::diagnostics::crash::fatal(format_args!("VFS mount root owner underflow"));
        }
    }
}

impl Node {
    pub(super) const fn id(&self) -> u64 {
        self.id
    }
    pub(super) fn attributes(&self) -> Result<NodeAttributes, Error> {
        let size = self
            .data
            .lock()
            .map_err(Error::Lock)?
            .contents
            .bytes()
            .len() as u64;
        Ok(self
            .metadata
            .with(|metadata| NodeAttributes::new(self.kind, metadata.mode, size)))
    }
    pub(super) fn metadata(&self) -> Result<(NodeAttributes, NodeMetadata), Error> {
        let data = self.data.lock().map_err(Error::Lock)?;
        Ok(self.metadata.with(|metadata| {
            (
                NodeAttributes::new(self.kind, metadata.mode, data.contents.bytes().len() as u64),
                *metadata,
            )
        }))
    }
    pub(super) fn set_metadata(&self, update: MetadataUpdate) -> Result<(), Error> {
        if update.mode.is_some_and(|mode| mode & !0o777 != 0) {
            return Err(Error::InvalidInput);
        }
        let now = crate::time::realtime();
        self.metadata.with(|metadata| {
            if let Some(mode) = update.mode {
                metadata.mode = mode;
            }
            if let Some(accessed) = update.accessed {
                metadata.accessed = Some(accessed);
            }
            if let Some(modified) = update.modified {
                metadata.modified = Some(modified);
            }
            if update.mode.is_some() || update.accessed.is_some() || update.modified.is_some() {
                metadata.changed = now;
            }
        });
        Ok(())
    }
    fn require(&self, kind: NodeKind) -> Result<(), Error> {
        if self.kind == kind {
            Ok(())
        } else {
            Err(match kind {
                NodeKind::Directory => Error::NotDirectory,
                NodeKind::Symlink => Error::NotSymlink,
                _ if self.kind == NodeKind::Directory => Error::IsDirectory,
                _ => Error::NotRegularFile,
            })
        }
    }
    fn linked(&self) -> bool {
        self.topology.with(|topology| topology.links != 0)
            || self.mount_roots.load(Ordering::Acquire) != 0
    }
    fn modified(&self, now: Option<Timestamp>) {
        self.metadata.with(|metadata| {
            metadata.modified = now;
            metadata.changed = now;
        });
    }
    fn changed(&self, now: Option<Timestamp>) {
        self.metadata.with(|metadata| metadata.changed = now);
    }
}

impl Ramfs {
    pub(super) fn empty(domain: ResourceDomain) -> Result<Self, Error> {
        let budget = Budget(domain);
        let charge = budget
            .reserve(FallibleArc::<Node>::allocation_size())
            .map_err(Error::Resource)?;
        let root = FallibleArc::try_new(Node {
            mount_pins: AtomicU64::new(0),
            mount_roots: AtomicU64::new(0),
            id: 0,
            kind: NodeKind::Directory,
            topology: SpinLock::new(Topology {
                links: 1,
                parent: None,
            }),
            metadata: SpinLock::new(NodeMetadata {
                mode: 0o755,
                accessed: None,
                modified: None,
                created: None,
                changed: None,
            }),
            data: Mutex::new(NodeData::new(FileData::borrowed(&[]))),
            children: Mutex::new(Children::new()),
            locks: super::locks::FileLocks::new(),
            content: super::file_data::FileContent::new(),
            archive: None,
            _charge: charge,
        })?;
        Ok(Self {
            root,
            next_id: AtomicU64::new(1),
            budget,
            mutation: Mutex::new(()),
            epoch: AtomicU64::new(0),
        })
    }

    pub(super) fn from_archive(
        archive: RamFs<'static>,
        backing: Option<FallibleArc<crate::boot::Archive>>,
    ) -> Result<Self, Error> {
        let budget = Budget(
            ResourceDomain::try_new_root(
                ResourceLimits::UNLIMITED
                    .with(ResourceKind::KernelMemoryBytes, STORAGE_LIMIT)
                    .with(
                        ResourceKind::CommittedPages,
                        STORAGE_LIMIT / hyper::mm::PAGE_SIZE,
                    ),
            )
            .map_err(Error::Resource)?,
        );
        let count = archive.nodes().len();
        let mut pending: Vec<Children> = Vec::new();
        pending
            .try_reserve_exact(count)
            .map_err(|_| Error::Allocation)?;
        pending.resize_with(count, Children::new);
        let mut root = None;
        let mut sources = Vec::new();
        sources
            .try_reserve_exact(count)
            .map_err(|_| Error::Allocation)?;
        sources.extend(archive.nodes());
        for source in sources.into_iter().rev() {
            let mut children =
                core::mem::replace(&mut pending[source.id().get() as usize], Children::new());
            children.entries.reverse();
            for (index, entry) in children.entries.iter_mut().enumerate() {
                entry.cookie = index as u64 + 1;
            }
            let charge = budget
                .reserve(FallibleArc::<Node>::allocation_size())
                .map_err(Error::Resource)?;
            let mut child_directories = Vec::new();
            child_directories
                .try_reserve_exact(children.entries.len())
                .map_err(|_| Error::Allocation)?;
            child_directories.extend(
                children
                    .entries
                    .iter()
                    .filter(|entry| entry.node.kind == NodeKind::Directory)
                    .map(|entry| entry.node.clone()),
            );
            let node = FallibleArc::try_new(Node {
                mount_pins: AtomicU64::new(0),
                mount_roots: AtomicU64::new(0),
                id: source.id().get(),
                kind: source.kind(),
                topology: SpinLock::new(Topology {
                    links: 1,
                    parent: None,
                }),
                metadata: SpinLock::new(NodeMetadata {
                    mode: source.mode() & 0o777,
                    accessed: None,
                    modified: source
                        .modified_seconds()
                        .and_then(|value| Timestamp::new(i64::from(value), 0)),
                    created: None,
                    changed: None,
                }),
                data: Mutex::new(NodeData::new(FileData::borrowed(source.data()))),
                children: Mutex::new(children),
                locks: super::locks::FileLocks::new(),
                content: super::file_data::FileContent::new(),
                archive: backing.clone(),
                _charge: charge,
            })?;
            // Startup cannot acquire sleeping locks. These retained children
            // allow parent hints to be completed before publishing the root.
            for child in child_directories {
                child
                    .topology
                    .with(|topology| topology.parent = Some(node.downgrade()));
            }
            if source.id().get() == 0 {
                root = Some(node);
            } else {
                let parent = &mut pending[source.parent().get() as usize];
                let cookie = parent.prepare_insert(&budget)?;
                parent.insert(
                    Name::new(source.name()).map_err(|_| Error::InvalidBackendResult)?,
                    node,
                    cookie,
                );
            }
        }
        Ok(Self {
            root: root.ok_or(Error::InvalidBackendResult)?,
            next_id: AtomicU64::new(count as u64),
            budget,
            mutation: Mutex::new(()),
            epoch: AtomicU64::new(0),
        })
    }
    /// Serialize the final topology publication with directory renames. The
    /// caller completes all allocation and backend I/O before this callback.
    pub(super) fn with_namespace_stable<T>(
        &self,
        epoch: u64,
        publish: impl FnOnce() -> Result<T, Error>,
    ) -> Result<T, Error> {
        let _guard = self.mutation.lock().map_err(Error::Lock)?;
        if epoch & 1 != 0 || self.epoch.load(Ordering::Acquire) != epoch {
            return Err(Error::Busy);
        }
        publish()
    }

    pub(super) fn pin_mount(&self, node: &FallibleArc<Node>) -> Result<MountPin, Error> {
        let _mutation = self.mutation.lock().map_err(Error::Lock)?;
        node.require(NodeKind::Directory)?;
        if !node.linked() {
            return Err(Error::Missing);
        }
        node.mount_pins
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                count.checked_add(1)
            })
            .map_err(|_| Error::IdentifierExhausted)?;
        Ok(MountPin(node.clone()))
    }

    pub(super) fn root(&self) -> FallibleArc<Node> {
        self.root.clone()
    }
    pub(super) fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::Acquire)
    }
    pub(super) fn wait_for_namespace(&self) -> Result<(), Error> {
        let _guard = self.mutation.lock().map_err(Error::Lock)?;
        Ok(())
    }
    pub(super) fn begin(&self, expected: Option<u64>) -> Result<Mutation<'_>, Error> {
        let guard = self.mutation.lock().map_err(Error::Lock)?;
        let epoch = self.epoch();
        if expected.is_some_and(|value| value != epoch) {
            return Err(Error::Busy);
        }
        let next = epoch.checked_add(2).ok_or(Error::IdentifierExhausted)?;
        self.epoch.store(epoch + 1, Ordering::Release);
        Ok(Mutation {
            _guard: guard,
            epoch: &self.epoch,
            committed_epoch: next,
        })
    }
    pub(super) fn lookup(
        &self,
        directory: &Node,
        name: Name<'_>,
    ) -> Result<Option<FallibleArc<Node>>, Error> {
        directory.require(NodeKind::Directory)?;
        let children = directory.children.lock().map_err(Error::Lock)?;
        Ok(children
            .index(name)
            .map(|index| children.entries[index].node.clone()))
    }
    pub(super) fn ancestry(
        &self,
        root: &FallibleArc<Node>,
        start: &FallibleArc<Node>,
        budget: &ScratchBudget,
    ) -> Result<ScratchVec<(FallibleArc<Node>, ScratchString)>, Error> {
        let mut reverse = ScratchVec::new(budget.clone());
        reverse.try_reserve_exact(1)?;
        let mut current = start.clone();
        let mut path_bytes = 0usize;
        while current.id != root.id {
            if reverse.len() == MAX_PATH_COMPONENTS {
                return Err(Error::InvalidInput);
            }
            let parent = current
                .topology
                .with(|topology| topology.parent.clone())
                .and_then(|weak| weak.upgrade())
                .ok_or(Error::Missing)?;
            let children = parent.children.lock().map_err(Error::Lock)?;
            let entry = children
                .entries
                .iter()
                .find(|entry| entry.node.id == current.id)
                .ok_or(Error::Missing)?;
            path_bytes = path_bytes
                .checked_add(entry.length + 1)
                .filter(|length| *length <= MAX_PATH_BYTES)
                .ok_or(Error::InvalidInput)?;
            let mut name = ScratchString::new(budget.clone());
            name.try_reserve_exact(entry.length)?;
            name.push_str(
                core::str::from_utf8(&entry.name[..entry.length])
                    .map_err(|_| Error::InvalidBackendResult)?,
            )?;
            reverse.try_reserve(1)?;
            reverse.push((current.clone(), name))?;
            drop(children);
            current = parent;
        }
        reverse.reverse();
        Ok(reverse)
    }
    pub(super) fn entry(
        &self,
        directory: &Node,
        cookie: u64,
    ) -> Result<Option<EntrySnapshot>, Error> {
        directory.require(NodeKind::Directory)?;
        let children = directory.children.lock().map_err(Error::Lock)?;
        if cookie >= children.next_cookie {
            return Err(Error::InvalidDirectoryCookie);
        }
        let Some(entry) = children
            .entries
            .iter()
            .filter(|entry| entry.cookie > cookie)
            .min_by_key(|entry| entry.cookie)
        else {
            return Ok(None);
        };
        Ok(Some(EntrySnapshot {
            name: entry.name,
            length: entry.length,
            attributes: entry.node.attributes()?,
            next_cookie: entry.cookie,
        }))
    }
    pub(super) fn read(
        &self,
        node: &Node,
        offset: u64,
        output: &mut [u8],
        link: bool,
    ) -> Result<usize, Error> {
        node.require(if link {
            NodeKind::Symlink
        } else {
            NodeKind::File
        })?;
        let count = node
            .data
            .lock()
            .map_err(Error::Lock)?
            .contents
            .read(offset, output);
        if count != 0 {
            node.metadata
                .with(|metadata| metadata.accessed = crate::time::realtime());
        }
        Ok(count)
    }
    pub(super) fn write(
        &self,
        node: &Node,
        offset: Option<u64>,
        input: &[u8],
    ) -> Result<(usize, u64), Error> {
        node.require(NodeKind::File)?;
        let mut data = node.data.lock().map_err(Error::Lock)?;
        let offset = offset.unwrap_or(data.contents.bytes().len() as u64);
        let actual = data
            .contents
            .write(offset, input, &DataBudget(&self.budget))
            .map_err(map_data_error)?;
        if actual != 0 {
            node.modified(crate::time::realtime());
        }
        Ok((actual, offset + actual as u64))
    }
    pub(super) fn resize(&self, node: &Node, length: u64) -> Result<(), Error> {
        node.require(NodeKind::File)?;
        let mut data = node.data.lock().map_err(Error::Lock)?;
        data.contents
            .resize(length, &DataBudget(&self.budget))
            .map_err(map_data_error)?;
        node.modified(crate::time::realtime());
        drop(data);
        Ok(())
    }
    pub(super) fn executable(
        &self,
        node: &Node,
        sponsor: &ResourceDomain,
    ) -> Result<Option<ExecutableSnapshot>, Error> {
        if node.kind != NodeKind::File {
            return Ok(None);
        }
        let data = node.data.lock().map_err(Error::Lock)?;
        if let Some(archive) = data.contents.archive() {
            return Ok(Some(ExecutableSnapshot::borrowed(
                archive,
                node.archive.clone(),
            )));
        }
        let charge = sponsor
            .reserve(ResourceAmount::ZERO.with(
                ResourceKind::KernelMemoryBytes,
                data.contents.bytes().len() as u64,
            ))
            .map_err(Error::Resource)?
            .commit();
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(data.contents.bytes().len())
            .map_err(|_| Error::Allocation)?;
        bytes.extend_from_slice(data.contents.bytes());
        Ok(Some(ExecutableSnapshot::owned(bytes, charge)))
    }

    pub(super) fn create<R, E: From<Error>>(
        &self,
        directory: &FallibleArc<Node>,
        name: EntryName<'_>,
        creation: Creation<'_>,
        epoch: Option<u64>,
        publish: impl FnOnce(FallibleArc<Node>) -> Result<R, E>,
    ) -> Result<R, E> {
        let Creation { kind, mode, target } = creation;
        if name.directory_required && kind != NodeKind::Directory {
            return Err(Error::NotDirectory.into());
        }
        let name = name.name;
        directory.require(NodeKind::Directory)?;
        let id = self
            .next_id
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                value.checked_add(1)
            })
            .map_err(|_| Error::IdentifierExhausted)?;
        let charge = self
            .budget
            .reserve(FallibleArc::<Node>::allocation_size())
            .map_err(Error::Resource)?;
        let now = crate::time::realtime();
        let mut data = FileData::borrowed(&[]);
        if let Some(target) = target {
            data.write(0, target, &DataBudget(&self.budget))
                .map_err(map_data_error)?;
        }
        let node = FallibleArc::try_new(Node {
            mount_pins: AtomicU64::new(0),
            mount_roots: AtomicU64::new(0),
            id,
            kind,
            topology: SpinLock::new(Topology {
                links: 1,
                parent: (kind == NodeKind::Directory).then(|| directory.downgrade()),
            }),
            metadata: SpinLock::new(NodeMetadata {
                mode,
                accessed: now,
                modified: now,
                created: now,
                changed: now,
            }),
            data: Mutex::new(NodeData::new(data)),
            children: Mutex::new(Children::new()),
            locks: super::locks::FileLocks::new(),
            content: super::file_data::FileContent::new(),
            archive: None,
            _charge: charge,
        })
        .map_err(Error::from)?;
        let mutation = self.begin(epoch)?;
        let mut children = directory.children.lock().map_err(Error::Lock)?;
        if !directory.linked() {
            return Err(Error::Missing.into());
        }
        if children.index(name).is_some() {
            return Err(Error::AlreadyExists.into());
        }
        let cookie = children.prepare_insert(&self.budget)?;
        let result = publish(node.clone())?;
        children.insert(name, node, cookie);
        directory.modified(now);
        drop(children);
        drop(mutation);
        Ok(result)
    }

    pub(super) fn open_existing<R, E: From<Error>>(
        &self,
        node: &FallibleArc<Node>,
        truncate: bool,
        publish: impl FnOnce(NodeAttributes) -> Result<R, E>,
    ) -> Result<R, E> {
        node.require(NodeKind::File)?;
        let mut data = node.data.lock().map_err(Error::Lock)?;

        let attributes = node.metadata.with(|metadata| {
            NodeAttributes::new(node.kind, metadata.mode, data.contents.bytes().len() as u64)
        });
        // A failed handle reservation/publication must leave file contents
        // untouched. All fallible work precedes this infallible replacement.
        let result = publish(attributes)?;
        let old = truncate
            .then(|| core::mem::replace(&mut *data, NodeData::new(FileData::borrowed(&[]))));
        if truncate {
            node.modified(crate::time::realtime());
        }
        drop(data);
        drop(old);
        Ok(result)
    }

    pub(super) fn remove(
        &self,
        directory: &Node,
        name: EntryName<'_>,
        kind: NodeKind,
        expected_node: Option<u64>,
        epoch: Option<u64>,
    ) -> Result<(), Error> {
        directory.require(NodeKind::Directory)?;
        let mutation = self.begin(epoch)?;
        let mut children = directory.children.lock().map_err(Error::Lock)?;
        let index = children.index(name.name).ok_or(Error::Missing)?;
        let node = &children.entries[index].node;
        if node.mount_pins.load(Ordering::Acquire) != 0 {
            return Err(Error::Busy);
        }
        if name.directory_required {
            node.require(NodeKind::Directory)?;
        }
        if expected_node.is_some_and(|id| id != node.id) {
            return Err(Error::Busy);
        }
        if kind == NodeKind::Directory {
            node.require(NodeKind::Directory)?;
            if !node
                .children
                .lock()
                .map_err(Error::Lock)?
                .entries
                .is_empty()
            {
                return Err(Error::NotEmpty);
            }
        } else if node.kind == NodeKind::Directory {
            return Err(Error::IsDirectory);
        }
        let removed = children.entries.remove(index);
        unlink_node(&removed.node);
        let now = crate::time::realtime();
        removed.node.changed(now);
        directory.modified(now);
        drop(children);
        drop(mutation);
        drop(removed);
        Ok(())
    }

    pub(super) fn link(
        &self,
        node: &FallibleArc<Node>,
        directory: &FallibleArc<Node>,
        name: EntryName<'_>,
        epoch: u64,
    ) -> Result<(), Error> {
        node.require(NodeKind::File)?;
        if name.directory_required {
            return Err(Error::NotDirectory);
        }
        let name = name.name;
        directory.require(NodeKind::Directory)?;
        let mutation = self.begin(Some(epoch))?;
        if !directory.linked() || !node.linked() {
            return Err(Error::Missing);
        }
        let mut children = directory.children.lock().map_err(Error::Lock)?;
        if children.index(name).is_some() {
            return Err(Error::AlreadyExists);
        }
        let links = node
            .topology
            .with(|topology| topology.links.checked_add(1))
            .ok_or(Error::IdentifierExhausted)?;
        let cookie = children.prepare_insert(&self.budget)?;
        children.insert(name, node.clone(), cookie);
        node.topology.with(|topology| topology.links = links);
        let now = crate::time::realtime();
        node.changed(now);
        directory.modified(now);
        drop(children);
        drop(mutation);
        Ok(())
    }

    pub(super) fn rename(
        &self,
        source: &FallibleArc<Node>,
        name: EntryName<'_>,
        destination: &FallibleArc<Node>,
        new_name: EntryName<'_>,
        epoch: u64,
    ) -> Result<(), Error> {
        source.require(NodeKind::Directory)?;
        destination.require(NodeKind::Directory)?;
        let mutation = self.begin(Some(epoch))?;
        if !source.linked() || !destination.linked() {
            return Err(Error::Missing);
        }
        let node = self.lookup(source, name.name)?.ok_or(Error::Missing)?;
        let replaced = self.lookup(destination, new_name.name)?;
        if node.mount_pins.load(Ordering::Acquire) != 0
            || replaced
                .as_ref()
                .is_some_and(|other| other.mount_pins.load(Ordering::Acquire) != 0)
        {
            return Err(Error::Busy);
        }
        if name.directory_required || new_name.directory_required {
            node.require(NodeKind::Directory)?;
        }
        if new_name.directory_required
            && let Some(other) = &replaced
        {
            other.require(NodeKind::Directory)?;
        }
        let name = name.name;
        let new_name = new_name.name;
        if replaced.as_ref().is_some_and(|other| other.id == node.id) {
            return Ok(());
        }
        if node.kind == NodeKind::Directory && source.id != destination.id {
            // All mutation paths preserve an acyclic parent forest. Walk to
            // its actual root: capability-relative path limits do not impose
            // a global filesystem depth limit. Floyd's check fails closed if
            // internal corruption violates the forest invariant.
            let mut ancestor = Some(destination.clone());
            let mut fast = Some(destination.clone());
            while let Some(current) = ancestor {
                if current.id == node.id {
                    return Err(Error::InvalidInput);
                }
                ancestor = parent_hint(&current);
                fast = fast
                    .and_then(|node| parent_hint(&node))
                    .and_then(|node| parent_hint(&node));
                if let (Some(slow), Some(fast)) = (&ancestor, &fast)
                    && slow.id == fast.id
                {
                    return Err(Error::InvalidBackendResult);
                }
            }
        }
        if let Some(other) = &replaced {
            if node.kind == NodeKind::Directory {
                other.require(NodeKind::Directory)?;
                if !other
                    .children
                    .lock()
                    .map_err(Error::Lock)?
                    .entries
                    .is_empty()
                {
                    return Err(Error::NotEmpty);
                }
            } else if other.kind == NodeKind::Directory {
                return Err(Error::IsDirectory);
            }
        }
        let removed = if source.id == destination.id {
            let mut children = source.children.lock().map_err(Error::Lock)?;
            let cookie = children.prepare_insert(&self.budget)?;
            let mut index = children.index(name).ok_or(Error::InvalidBackendResult)?;
            let replaced_index = children.index(new_name);
            let removed =
                replaced_index.map(|replaced_index| children.entries.remove(replaced_index));
            if replaced_index.is_some_and(|replaced_index| replaced_index < index) {
                index -= 1;
            }
            let moved = children.entries.remove(index);
            children.insert(new_name, moved.node, cookie);
            removed
        } else {
            let mut from = source.children.lock().map_err(Error::Lock)?;
            let mut to = destination.children.lock().map_err(Error::Lock)?;
            let cookie = to.prepare_insert(&self.budget)?;
            let source_index = from.index(name).ok_or(Error::InvalidBackendResult)?;
            let removed = to.index(new_name).map(|index| to.entries.remove(index));
            let moved = from.entries.remove(source_index);
            to.insert(new_name, moved.node, cookie);
            if node.kind == NodeKind::Directory {
                node.topology
                    .with(|topology| topology.parent = Some(destination.downgrade()));
            }
            removed
        };
        let now = crate::time::realtime();
        if let Some(entry) = &removed {
            unlink_node(&entry.node);
            entry.node.changed(now);
        }
        node.changed(now);
        source.modified(now);
        destination.modified(now);
        drop(mutation);
        drop(removed);
        drop(replaced);
        Ok(())
    }
}

fn parent_hint(node: &Node) -> Option<FallibleArc<Node>> {
    node.topology
        .with(|topology| topology.parent.clone())
        .and_then(|parent| parent.upgrade())
}

fn unlink_node(node: &Node) {
    node.topology.with(|topology| {
        let Some(remaining) = topology.links.checked_sub(1) else {
            hyper::debug::invariant_failure("vfs::ramfs::unlink_node invariant");
        };
        topology.links = remaining;
        if remaining == 0 {
            topology.parent = None;
        }
    });
}

fn map_data_error(error: crate::fs::primitives::file_data::Error<ResourceError>) -> Error {
    match error {
        crate::fs::primitives::file_data::Error::Allocation => Error::Allocation,
        crate::fs::primitives::file_data::Error::Size => Error::InvalidSize,
        crate::fs::primitives::file_data::Error::Budget(error) => Error::Resource(error),
    }
}
