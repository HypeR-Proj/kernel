// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Userspace filesystem namespace adapter. Backend transactions serialize RPC
//! separately from short cache and binding state access. Live leases prevent
//! unlink; namespace transactions retain the backend gate through publication.

use super::CachePage;
use super::MetadataUpdate;
use super::file_record::FileRecord;
use super::instance::NodeMetadata;
use super::instance::{Creation, EntryName, Error};
use super::scratch::{ScratchBudget, ScratchString, ScratchVec};
use crate::sync::Mutex;
use alloc::{boxed::Box, string::String, vec::Vec};
use core::num::NonZeroU64;
use core::sync::atomic::{AtomicU64, Ordering};
use hyper_filesystem::protocol::{Entry, Error as BackendError};
use {
    crate::accounting::CommittedCharge, crate::accounting::ResourceAmount,
    crate::accounting::ResourceDomain, crate::accounting::ResourceDomainId,
    crate::accounting::ResourceError, crate::accounting::ResourceKind,
};
use {
    crate::fs::cache::ContentRevision, crate::fs::cache::FileDataCache,
    crate::fs::cache::FileIdentity, crate::fs::cache::FilesystemGeneration,
    crate::fs::cache::NodeIdentity,
};
mod transport;
use hyper::mm::{FallibleArc, WeakFallibleArc};
pub(in crate::fs::vfs) use transport::Health;
pub(super) use transport::Transport;
use {
    crate::fs::primitives::MAX_PATH_BYTES, crate::fs::primitives::Name,
    crate::fs::primitives::NodeAttributes, crate::fs::primitives::NodeKind,
};

mod cache;
mod directory;
mod records;
mod session;
mod shared;
mod writeback;
use records::{PreparedRecord, Records};
use session::{Backend, Session};
#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
#[path = "../../../../tests/kernel/vfs_records.rs"]
mod records_test;
#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
pub(super) use records_test::run as test_record_storage;

pub(super) struct Node {
    mount_pins: AtomicU64,
    // Serialized by the backend gate; separate from file content revisions.
    directory_revision: AtomicU64,
    pub(super) locks: super::locks::FileLocks,
    pub(super) record: FallibleArc<FileRecord>,
    _charge: CommittedCharge,
}
impl Node {
    pub(super) fn id(&self) -> u64 {
        self.record.id()
    }

    fn invalidate_directory(&self) -> Result<(), Error> {
        self.directory_revision
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |revision| {
                revision.checked_add(1)
            })
            .map(|_| ())
            .map_err(|_| Error::IdentifierExhausted)
    }
}
pub(super) struct MountPin(FallibleArc<Node>);
impl Drop for MountPin {
    fn drop(&mut self) {
        self.0.mount_pins.fetch_sub(1, Ordering::Release);
    }
}
struct Record {
    id: u64,
    metadata_revision: u64,
    directory_revision: u64,
    path: String,
    node: WeakFallibleArc<Node>,
    content: WeakFallibleArc<FileRecord>,
    _charge: CommittedCharge,
}
struct State {
    health: FallibleArc<transport::Health>,
    writeback: writeback::Writeback,
    metadata: Box<Entry>,
    records: Records,
    next_id: u64,
    cache: FallibleArc<FileDataCache<CachePage>>,
    generation: FilesystemGeneration,
    metadata_revision: u64,
    root_record: FallibleArc<FileRecord>,
}
pub(super) struct RemoteFs {
    state: Mutex<State>,
    backend: Mutex<Box<Backend>>,
    root: FallibleArc<Node>,
    pub(in crate::fs::vfs) domain: ResourceDomain,
    shared: Option<super::shared::SharedCache>,
    epoch: AtomicU64,
    health: FallibleArc<transport::Health>,
    _charge: CommittedCharge,
}
struct Mutation<'a>(&'a AtomicU64, u64);
impl Drop for Mutation<'_> {
    fn drop(&mut self) {
        self.0.store(self.1, Ordering::Release);
    }
}
impl RemoteFs {
    #[inline(never)]
    pub(super) fn mount(
        mut volume: Transport,
        domain: ResourceDomain,
        generation: FilesystemGeneration,
        cache: FallibleArc<FileDataCache<CachePage>>,
    ) -> Result<FallibleArc<Self>, Error> {
        let persistent = charge(
            &domain,
            FallibleArc::<Self>::allocation_size()
                + core::mem::size_of::<Backend>()
                + 2 * core::mem::size_of::<Entry>()
                + MAX_PATH_BYTES
                + hyper_filesystem::protocol::DIRECTORY_BATCH_BYTES,
        )?;
        // Reserve the final allocation before preparing the mount. Passing a
        // completed mount through the fallible Arc constructor keeps another
        // full value alive across allocation and its failure cleanup.
        let slot = hyper::mm::UniqueFallibleArc::try_new_uninit()?;
        let metadata = metadata_scratch()?;
        let root = node(FileRecord::try_new(0, &domain)?, &domain)?;
        let health = volume.health.clone();
        let writeback = writeback::Writeback::new(volume.take_writeback().map_err(map)?, &domain)?;
        let shared = if writeback.enabled() {
            Some(super::shared::SharedCache::new(&domain)?)
        } else {
            None
        };
        let backend = Backend::new(volume, &domain, writeback.enabled())?;
        let mut state = State {
            health: health.clone(),
            writeback,
            metadata,
            records: Records::new(),
            next_id: 1,
            cache,
            generation,
            metadata_revision: 1,
            root_record: root.record.clone(),
        };
        state.records.insert(Records::prepare(Record {
            id: 0,
            metadata_revision: 1,
            directory_revision: 1,
            path: String::new(),
            node: root.downgrade(),
            content: root.record.downgrade(),
            _charge: record_charge(&domain)?,
        })?);
        Ok(slot
            .write(Self {
                state: Mutex::new(state),
                backend: Mutex::new(backend),
                root,
                domain,
                shared,
                epoch: AtomicU64::new(0),
                health,
                _charge: persistent,
            })
            .into_shared())
    }
    #[inline(never)]
    /// Serialize the final topology publication with directory renames. The
    /// caller completes all allocation and backend I/O before this callback.
    pub(super) fn with_namespace_stable<T>(
        &self,
        epoch: u64,
        publish: impl FnOnce() -> Result<T, Error>,
    ) -> Result<T, Error> {
        let _backend = self.backend.lock().map_err(Error::Lock)?;
        let _guard = self.state.lock().map_err(Error::Lock)?;
        if epoch & 1 != 0 || self.epoch.load(Ordering::Acquire) != epoch {
            return Err(Error::Busy);
        }
        publish()
    }

    pub(super) fn pin_mount(&self, node: &FallibleArc<Node>) -> Result<MountPin, Error> {
        let mut state = self.session()?;
        let _paths = state.scratch_charge(&self.domain, MAX_PATH_BYTES)?;
        let path = state.path(node)?;
        if !state.is_directory(&path)? {
            return Err(Error::NotDirectory);
        }
        node.mount_pins
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_add(1))
            .map_err(|_| Error::IdentifierExhausted)?;
        Ok(MountPin(node.clone()))
    }
    pub(super) fn root(&self) -> FallibleArc<Node> {
        self.root.clone()
    }
    pub(super) fn unmount_with(
        &self,
        publish: impl FnOnce() -> Result<(), Error>,
    ) -> Result<(), Error> {
        self.flush_shared(None, true)?;
        // Admission is frozen. This lock also drains read-ahead which retains
        // only weak content identity rather than a namespace Location.
        let mut state = self.session()?;
        state.fence_cache();
        state.sync_writeback()?;
        publish()?;
        state.retire_writeback();
        // No fallible operation follows publication. A late speculative reader
        // sees closed health before it can initiate a new backend transaction.
        self.health.fail();
        Ok(())
    }
    pub(super) fn read_status(&self) -> Result<(), Error> {
        self.health.check().map_err(map)
    }
    pub(in crate::fs::vfs) fn atomic_health(&self) -> FallibleArc<Health> {
        self.health.clone()
    }
    pub(super) fn abandon_with(
        &self,
        publish: impl FnOnce() -> Result<(), Error>,
    ) -> Result<u64, Error> {
        // This path is explicitly lossy and only for already failed providers.
        // It performs no backend RPC. The state mutex drains local range borrows;
        // block request leases independently retain any device-visible storage.
        if self.health.check().is_ok() {
            return Err(Error::Busy);
        }
        let shared_discarded = self.revoke_failed_shared()?;
        let mut state = self.session()?;
        publish()?;
        self.health.fail();
        let discarded = state.abandon_writeback().saturating_add(shared_discarded);
        state.backend.batches.abandon();
        Ok(discarded)
    }
    pub(super) fn reserve_read_ahead_metadata(
        &self,
        bytes: usize,
    ) -> Result<CommittedCharge, Error> {
        // Speculation must not initiate quota reclaim or wait for volume state.
        charge(&self.domain, bytes)
    }
    pub(super) fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::Acquire)
    }
    pub(super) fn wait_for_namespace(&self) -> Result<(), Error> {
        drop(self.backend.lock().map_err(Error::Lock)?);
        Ok(())
    }

    /// Scheduled housekeeping only: no media I/O and no waiting for a busy
    /// namespace transaction. Page reclaim has already released content pins.
    pub(super) fn reclaim_records(&self, limit: usize) -> (usize, usize) {
        let mut inspected = 0;
        let mut reclaimed = 0;
        let mut remaining = None;
        while inspected < limit {
            let retired = {
                let Ok(Some(mut state)) = self.state.try_lock() else {
                    break;
                };
                let remaining = remaining.get_or_insert(state.records.len());
                if *remaining == 0 {
                    break;
                }
                *remaining -= 1;
                let Some(retired) = state.records.inspect_next() else {
                    break;
                };
                retired
            };
            inspected += 1;
            if let Some(record) = retired {
                reclaimed += 1;
                // Release path storage, weak headers, and quota after the
                // namespace lock, independently of cache/allocator locks.
                drop(record);
            }
        }
        (inspected, reclaimed)
    }

    /// Finish one finite cleanup pass without waiting for a namespace owner.
    /// A quota requester can hold this volume's mutex; it prunes its own dead
    /// bindings after the page worker completes instead.
    pub(super) fn reclaim_idle_records(&self, domain: Option<ResourceDomainId>) {
        if domain.is_some_and(|id| !self.domain.charges_domain(id)) {
            return;
        }
        let mut remaining = match self.state.try_lock() {
            Ok(Some(state)) => state.records.cleanup_bound(),
            _ => return,
        };
        while remaining != 0 {
            let (inspected, _) = self.reclaim_records(remaining.min(64));
            if inspected == 0 {
                break;
            }
            remaining -= inspected;
            if crate::task::scheduler::cond_resched().is_err() {
                break;
            }
        }
    }
    fn mutation(&self, expected: Option<u64>) -> Result<Mutation<'_>, Error> {
        let epoch = self.epoch();
        if expected.is_some_and(|v| v != epoch) {
            return Err(Error::Busy);
        }
        let next = epoch.checked_add(2).ok_or(Error::IdentifierExhausted)?;
        self.epoch.store(epoch + 1, Ordering::Release);
        Ok(Mutation(&self.epoch, next))
    }
    #[inline(never)]
    pub(super) fn attributes(&self, node: &Node) -> Result<NodeAttributes, Error> {
        let mut state = self.session()?;
        let _paths = state.scratch_charge(&self.domain, MAX_PATH_BYTES)?;
        let path = state.path(node)?;
        let entry = state.stat(&path)?;
        Ok(attributes(entry))
    }
    #[inline(never)]
    pub(super) fn metadata(&self, node: &Node) -> Result<(NodeAttributes, NodeMetadata), Error> {
        let mut state = self.session()?;
        let _paths = state.scratch_charge(&self.domain, MAX_PATH_BYTES)?;
        let path = state.path(node)?;
        let entry = state.stat(&path)?;
        let attributes = attributes(entry);
        Ok((
            attributes,
            NodeMetadata {
                mode: attributes.mode(),
                accessed: entry.accessed.and_then(to_kernel_time),
                modified: entry.modified.and_then(to_kernel_time),
                created: entry.created.and_then(to_kernel_time),
                changed: None,
            },
        ))
    }
    #[inline(never)]
    pub(super) fn set_metadata(&self, node: &Node, update: MetadataUpdate) -> Result<(), Error> {
        self.flush_shared(Some(node.id()), false)?;
        let mut state = self.session()?;
        state.fence_cache();
        state.flush_writeback()?;
        let _paths = state.scratch_charge(&self.domain, MAX_PATH_BYTES)?;
        let path = state.path(node)?;
        let entry = state.stat(&path)?;
        if update
            .mode
            .is_some_and(|mode| mode != attributes(entry).mode())
        {
            return Err(Error::Unsupported);
        }
        if update.accessed.is_none() && update.modified.is_none() {
            return Ok(());
        }
        state.invalidate_entry(node.id())?;
        state.rpc(|volume| {
            volume.set_times(
                &path,
                update.accessed.map(to_service_time),
                update.modified.map(to_service_time),
            )
        })
    }
    #[inline(never)]
    pub(super) fn lookup(
        &self,
        directory: &Node,
        name: Name<'_>,
    ) -> Result<Option<FallibleArc<Node>>, Error> {
        let mut state = self.session()?;
        let _paths = state.scratch_charge(&self.domain, MAX_PATH_BYTES * 3)?;
        let parent = state.path(directory)?;
        if !state.is_directory(&parent)? {
            return Err(Error::NotDirectory);
        }
        let path = child(&parent, name)?;
        let entry = match state.stat(&path) {
            Ok(entry) => entry,
            Err(Error::Missing) => return Ok(None),
            Err(e) => return Err(e),
        };
        // Use the stored spelling so differently cased opens share one lease.
        let canonical = join(&parent, entry.name())?;
        let node = state.lease(canonical, &self.domain)?;
        // A lookup can create the canonical binding after an unbound name hit.
        // Retain its validated attributes under the new identity so open does not
        // immediately repeat Stat. Existing bound entries remain immutable.
        let path = state.records.path(node.id())?;
        let identity = state.entry_identity(path)?;
        cache::retain_entry(
            &state.cache,
            identity,
            path,
            &state.metadata,
            &state.root_record,
        );
        Ok(Some(node))
    }
    #[inline(never)]
    pub(super) fn read(
        &self,
        node: &Node,
        offset: u64,
        destination: &mut [u8],
        symlink: bool,
    ) -> Result<usize, Error> {
        let mut state = self.session()?;
        let _paths = state.scratch_charge(&self.domain, MAX_PATH_BYTES)?;
        let path = state.path(node)?;
        if symlink {
            if state.stat(&path)?.kind != hyper_filesystem::protocol::SYMLINK {
                return Err(Error::NotSymlink);
            }
            state.rpc(|volume| volume.read_link(&path, destination))
        } else {
            state.rpc(|volume| volume.read_at(&path, offset, destination))
        }
    }
    pub(super) fn read_scatter(
        &self,
        node: &Node,
        offset: u64,
        output: &mut [&mut [u8]],
    ) -> Result<usize, Error> {
        let mut state = self.session()?;
        let _paths = state.scratch_charge(&self.domain, MAX_PATH_BYTES)?;
        let path = state.path(node)?;
        state.rpc(|volume| volume.read_scatter(&path, offset, output))
    }

    pub(super) fn try_read_scatter(
        &self,
        record: &FileRecord,
        offset: u64,
        output: &mut [&mut [u8]],
    ) -> Result<Option<usize>, Error> {
        let Some(mut state) = self.try_session()? else {
            return Ok(None);
        };
        state.bind_path(record.id())?;
        state
            .rpc_bound(|volume, path| volume.read_scatter(path, offset, output))
            .map(Some)
    }
    pub(super) fn try_read(
        &self,
        record: &FileRecord,
        offset: u64,
        output: &mut [u8],
    ) -> Result<Option<usize>, Error> {
        let Some(mut state) = self.try_session()? else {
            return Ok(None);
        };
        state.bind_path(record.id())?;
        state
            .rpc_bound(|volume, path| volume.read_at(path, offset, output))
            .map(Some)
    }
    #[inline(never)]
    pub(super) fn write(
        &self,
        node: &Node,
        offset: Option<u64>,
        input: &super::write_source::WriteSource<'_>,
        content: &mut super::file_data::ContentGuard<'_>,
    ) -> Result<(usize, u64), Error> {
        content.check_writable()?;
        let mut first = offset;
        let mut done = 0;
        loop {
            let step = (|| -> Result<(usize, bool), Error> {
                // This path never waits for backend ownership. The same canonical
                // reclaimable attributes and versioned cache serve both paths.
                {
                    let mut state = self.state.lock().map_err(Error::Lock)?;
                    state.check_writeback()?;
                    if state.writeback.enabled()
                        && !state.writeback.fenced
                        && state.cached_stat_node(node.id())?
                    {
                        let length = state.metadata.size;
                        if state.metadata.directory() {
                            return Err(Error::IsDirectory);
                        }
                        if state.metadata.read_only() {
                            return Err(Error::Remote(BackendError::ReadOnly));
                        }
                        let base = *first.get_or_insert(length);
                        base.checked_add(input.len() as u64)
                            .ok_or(Error::InvalidInput)?;
                        if input.len() == 0 {
                            return Ok((0, false));
                        }
                        let at = base + done as u64;
                        if at <= length {
                            let chunk = input.slice(
                                done,
                                (input.len() - done).min(hyper_filesystem::protocol::DATA_BYTES),
                            )?;
                            let identity = FileIdentity::new(
                                state.generation,
                                NodeIdentity::new(
                                    NonZeroU64::new(node.id()).ok_or(Error::NotRegularFile)?,
                                ),
                                content.revision(),
                            );
                            if let Some(written) =
                                state.cache_write(node, identity, length, at, &chunk, None)?
                            {
                                content.set_length(length.max(at + written as u64));
                                return Ok((written, true));
                            }
                        }
                    }
                }
                // Never acquire the backend gate while retaining State. A slow
                // stat, partial-page miss or pressure fallback may wait here.
                let mut state = self.session()?;
                state.check_writeback()?;
                let _paths = state.scratch_charge(&self.domain, MAX_PATH_BYTES)?;
                let path = state.path(node)?;
                let entry = state.stat(&path)?;
                if entry.directory() {
                    return Err(Error::IsDirectory);
                }
                if entry.read_only() {
                    return Err(Error::Remote(BackendError::ReadOnly));
                }
                let length = entry.size;
                let base = *first.get_or_insert(length);
                base.checked_add(input.len() as u64)
                    .ok_or(Error::InvalidInput)?;
                if input.len() == 0 {
                    return Ok((0, false));
                }
                let at = base + done as u64;
                let chunk = input.slice(
                    done,
                    (input.len() - done).min(hyper_filesystem::protocol::DATA_BYTES),
                )?;
                if state.writeback.enabled() && at <= length {
                    let identity = FileIdentity::new(
                        state.generation,
                        NodeIdentity::new(NonZeroU64::new(node.id()).ok_or(Error::NotRegularFile)?),
                        content.revision(),
                    );
                    if let Some(written) =
                        state.cache_write_filled(node, identity, length, at, &chunk)?
                    {
                        content.set_length(length.max(at + written as u64));
                        return Ok((written, true));
                    }
                }
                state.fence_cache();
                state.flush_node(node.id())?;
                state.invalidate_entry(node.id())?;
                content.begin_mutation()?;
                let written = state.rpc(|volume| volume.write_at(&path, at, &chunk))?;
                content.set_length(length.max(at + written as u64));
                Ok((written, written == chunk.len()))
            })();
            match step {
                Ok((written, more)) => {
                    done += written;
                    if !more || done == input.len() {
                        break;
                    }
                }
                Err(_) if done != 0 => break,
                Err(error) => return Err(error),
            }
            if crate::task::scheduler::cond_resched().is_err() {
                if done != 0 {
                    break;
                }
                return Err(Error::InvalidInput);
            }
        }
        Ok((done, first.ok_or(Error::InvalidInput)? + done as u64))
    }

    #[inline(never)]
    pub(super) fn resize(&self, node: &Node, length: u64) -> Result<(), Error> {
        let mut state = self.session()?;
        state.fence_cache();
        state.flush_writeback()?;
        let _paths = state.scratch_charge(&self.domain, MAX_PATH_BYTES)?;
        let path = state.path(node)?;
        let entry = state.stat(&path)?;
        if entry.directory() {
            return Err(Error::IsDirectory);
        }
        if entry.read_only() {
            return Err(Error::Remote(BackendError::ReadOnly));
        }
        state.invalidate_entry(node.id())?;
        state.rpc(|volume| volume.resize(&path, length))
    }
    #[inline(never)]
    pub(super) fn sync(&self, node: &Node, scope: u64) -> Result<(), Error> {
        let volume_scope = scope == hyper::abi::native::HYPER_NATIVE_FILE_SYNC_VOLUME
            || self.attributes(node)?.kind() == NodeKind::Directory;
        self.flush_shared(if volume_scope { None } else { Some(node.id()) }, false)?;
        let mut state = self.session()?;
        state.check_writeback()?;
        if scope == hyper::abi::native::HYPER_NATIVE_FILE_SYNC_VOLUME {
            return state.sync_writeback();
        }
        let mode =
            hyper_filesystem::protocol::SyncMode::decode(scope).ok_or(Error::InvalidInput)?;
        let _paths = state.scratch_charge(&self.domain, MAX_PATH_BYTES)?;
        let path = state.path(node)?;
        if state.stat(&path)?.directory() {
            state.sync_writeback()
        } else {
            state.sync_file_writeback(node.id(), mode)
        }
    }
    #[inline(never)]
    pub(super) fn create<R, E: From<Error>>(
        &self,
        directory: &FallibleArc<Node>,
        name: EntryName<'_>,
        creation: Creation<'_>,
        epoch: Option<u64>,
        publish: impl FnOnce(FallibleArc<Node>) -> Result<R, E>,
    ) -> Result<R, E> {
        if creation.target.is_some()
            || !matches!(creation.kind, NodeKind::File | NodeKind::Directory)
        {
            return Err(Error::Unsupported.into());
        }
        if name.directory_required && creation.kind != NodeKind::Directory {
            return Err(Error::NotDirectory.into());
        }
        let mut state = self.session()?;
        let _paths = state.scratch_charge(&self.domain, MAX_PATH_BYTES * 3)?;
        let _mutation = self.mutation(epoch)?;
        let parent = state.path(directory)?;
        if !state.is_directory(&parent)? {
            return Err(Error::NotDirectory.into());
        }
        let path = child(&parent, name.name)?;
        // Prepare every owner and binding allocation before touching the directory.
        // The callback prepares a handle; it must not perform backend I/O.
        // A failed callback or create drops this unpublished binding, so later
        // lookup cannot inherit a ghost identity from a failed create.
        let id = state.next_id;
        state.next_id = id.checked_add(1).ok_or(Error::IdentifierExhausted)?;
        let (lease, record) = state.prepare_node(id, path, &self.domain)?;
        let prepared = publish(lease)?;
        directory.invalidate_directory()?;
        state.invalidate_metadata()?;
        state.rpc(|volume| {
            volume.create(&record.value().path, creation.kind == NodeKind::Directory)
        })?;
        // Creation succeeded while the namespace mutex excluded competing
        // lookups. Retire any expired name binding before publishing this one.
        if let Some(previous) = state.records.find_path(&record.value().path) {
            let id = previous.id;
            drop(state.records.remove(id));
        }
        state.records.insert(record);
        Ok(prepared)
    }
    #[inline(never)]
    pub(super) fn open_existing<R, E: From<Error>>(
        &self,
        node: &FallibleArc<Node>,
        truncate: bool,
        publish: impl FnOnce(NodeAttributes) -> Result<R, E>,
    ) -> Result<R, E> {
        let mut state = self.session()?;
        let _paths = state.scratch_charge(&self.domain, MAX_PATH_BYTES)?;
        let path = state.path(node)?;
        let entry = state.stat(&path)?;
        if entry.directory() {
            return Err(Error::IsDirectory.into());
        }
        let read_only = entry.read_only();
        let attributes = attributes(entry);
        let prepared = publish(attributes)?;
        if truncate {
            state.fence_cache();
            state.flush_writeback()?;
            if read_only {
                return Err(Error::Remote(BackendError::ReadOnly).into());
            }
            state.invalidate_entry(node.id())?;
            state.rpc(|volume| volume.resize(&path, 0))?;
        }
        Ok(prepared)
    }

    #[inline(never)]
    pub(super) fn remove(
        &self,
        directory: &Node,
        name: EntryName<'_>,
        kind: NodeKind,
        expected: Option<u64>,
        epoch: Option<u64>,
    ) -> Result<(), Error> {
        let shared_target = self.shared_binding(directory, name)?;
        if let Some(node) = shared_target {
            self.flush_shared(Some(node), true)?;
        }
        let mut state = self.session()?;
        state.fence_cache();
        state.flush_writeback()?;
        let _paths = state.scratch_charge(&self.domain, MAX_PATH_BYTES * 3)?;
        let _mutation = self.mutation(epoch)?;
        let parent = state.path(directory)?;
        let candidate = child(&parent, name.name)?;
        let entry = state.stat(&candidate)?;
        let path = join(&parent, entry.name())?;
        if name.directory_required && !entry.directory() {
            return Err(Error::NotDirectory);
        }
        if kind == NodeKind::Directory && !entry.directory() {
            return Err(Error::NotDirectory);
        }
        if kind != NodeKind::Directory && entry.directory() {
            return Err(Error::IsDirectory);
        }
        if state.records.find_path(&path).is_some_and(|record| {
            record.node.upgrade().is_some() || expected.is_some_and(|id| record.id != id)
        }) {
            return Err(Error::Busy);
        }
        let observed_target = state.records.find_path(&path).map(|record| record.id);
        if observed_target != shared_target
            || observed_target.is_some_and(|id| expected.is_some_and(|expected| expected != id))
        {
            return Err(Error::Busy);
        }
        if let Some(id) = observed_target
            && self.shared_has_resident(id)?
        {
            return Err(Error::Busy);
        }
        directory.invalidate_directory()?;
        state.invalidate_metadata()?;
        state.rpc(|volume| volume.remove(&path))?;
        // Cached content is not an active lease. Its old numeric identity may
        // remain in clean pages until eviction, but the removed name must never
        // reconnect it to a new file (or a recreated directory subtree).
        if kind == NodeKind::Directory {
            state
                .records
                .retain(|record| record.path != path && !within(&record.path, &path));
        } else if let Some(record) = state.records.find_path(&path) {
            // Canonical bindings cannot have descendants beneath a file.
            let id = record.id;
            drop(state.records.remove(id));
        }
        Ok(())
    }
    #[inline(never)]
    pub(super) fn link(
        &self,
        _node: &Node,
        _directory: &Node,
        _name: EntryName<'_>,
        _epoch: u64,
    ) -> Result<(), Error> {
        Err(Error::Unsupported)
    }
    #[inline(never)]
    pub(super) fn rename(
        &self,
        source: &Node,
        name: EntryName<'_>,
        destination: &Node,
        new_name: EntryName<'_>,
        epoch: u64,
    ) -> Result<(), Error> {
        let shared_source = self.shared_binding(source, name)?;
        let shared_target = self.shared_binding(destination, new_name)?;
        if let Some(target) = shared_target
            && Some(target) != shared_source
        {
            self.flush_shared(Some(target), true)?;
        }
        self.flush_shared(None, false)?;
        let mut state = self.session()?;
        state.fence_cache();
        state.flush_writeback()?;
        let _paths = state.scratch_charge(&self.domain, MAX_PATH_BYTES * 6)?;
        let _mutation = self.mutation(Some(epoch))?;
        let parent = state.path(source)?;
        let candidate = child(&parent, name.name)?;
        let entry = state.stat(&candidate)?;
        let old = join(&parent, entry.name())?;
        let directory = entry.directory();
        let original_name = copy(entry.name())?;
        let destination_path = state.path(destination)?;
        if !state.is_directory(&destination_path)? {
            return Err(Error::NotDirectory);
        }
        let new = child(&destination_path, new_name.name)?;
        if (name.directory_required || new_name.directory_required) && !directory {
            return Err(Error::NotDirectory);
        }
        // A backend may resolve aliases (for example case-folded names) to
        // the same canonical entry. Preserve that spelling in the registry,
        // otherwise the next canonical lookup creates a second lock identity.
        let (same_name, target_path) = match state.stat(&new) {
            Ok(entry) => (
                entry.name() == original_name,
                Some(join(&destination_path, entry.name())?),
            ),
            Err(Error::Missing) => (false, None),
            Err(error) => return Err(error),
        };
        if parent == destination_path && same_name {
            return Ok(());
        }
        let observed_target = target_path
            .as_ref()
            .and_then(|path| state.records.find_path(path))
            .map(|record| record.id);
        if observed_target != shared_target {
            return Err(Error::Busy);
        }
        if let Some(target) = observed_target
            && (state
                .records
                .get(target)
                .is_some_and(|record| record.node.is_alive())
                || self.shared_has_resident(target)?)
        {
            return Err(Error::Busy);
        }
        if within(&new, &old) {
            return Err(Error::InvalidInput);
        }
        let _scratch = state.reclaim_retry(|state| {
            let bytes = state
                .records
                .len()
                .checked_mul(MAX_PATH_BYTES + core::mem::size_of::<(u64, String)>())
                .ok_or(Error::Allocation)?;
            charge(&self.domain, bytes)
        })?;
        let mut replacements = Vec::new();
        replacements
            .try_reserve_exact(state.records.len())
            .map_err(|_| Error::Allocation)?;
        for record in state.records.iter() {
            if record.path == old || within(&record.path, &old) {
                if record
                    .node
                    .upgrade()
                    .is_some_and(|node| node.mount_pins.load(Ordering::Acquire) != 0)
                {
                    return Err(Error::Busy);
                }
                let suffix = &record.path[old.len()..];
                let length = new
                    .len()
                    .checked_add(suffix.len())
                    .filter(|n| *n <= MAX_PATH_BYTES)
                    .ok_or(Error::InvalidInput)?;
                let mut path = String::new();
                path.try_reserve_exact(length)
                    .map_err(|_| Error::Allocation)?;
                path.push_str(&new);
                path.push_str(suffix);
                replacements.push((record.id, path));
            }
        }
        source.invalidate_directory()?;
        if source.id() != destination.id() {
            destination.invalidate_directory()?;
        }
        // A moved directory's physical checkpoint still names the same media,
        // but its trusted path binding changes. Invalidate active descendant
        // streams before committing that change, without retaining extra nodes.
        for (id, _) in &replacements {
            if let Some(node) = state
                .records
                .get(*id)
                .and_then(|record| record.node.upgrade())
            {
                node.invalidate_directory()?;
            }
        }
        state.invalidate_metadata()?;
        state.rpc(|volume| volume.rename(&old, &new))?;
        if let Some(target) = observed_target {
            drop(state.records.remove(target));
        }
        // All replacement paths and index nodes already exist. Reindexing is
        // allocation-free after the backend has committed the rename.
        for (id, path) in replacements {
            state.records.rename_retained(id, path);
        }
        Ok(())
    }
    #[inline(never)]
    pub(super) fn ancestry<T>(
        &self,
        root: &FallibleArc<Node>,
        start: &FallibleArc<Node>,
        budget: &ScratchBudget,
        output: &mut ScratchVec<(T, ScratchString)>,
        wrap: impl Fn(FallibleArc<Node>) -> T,
    ) -> Result<(), Error> {
        let mut state = self.session()?;
        let _paths = state.scratch_charge(&self.domain, MAX_PATH_BYTES * 5)?;
        let root_path = state.path(root)?;
        let start_path = state.path(start)?;
        if root_path != start_path && !within(&start_path, &root_path) {
            return Err(Error::Missing);
        }
        let mut path = copy(&root_path)?;
        for component in start_path[root_path.len()..]
            .split('/')
            .filter(|s| !s.is_empty())
        {
            path = join(&path, component)?;
            let node = state.lease(copy(&path)?, &self.domain)?;
            let mut name = ScratchString::new(budget.clone());
            name.try_reserve_exact(component.len())?;
            name.push_str(component)?;
            output.try_reserve(1)?;
            output.push((wrap(node), name))?;
        }
        Ok(())
    }
}
impl State {
    fn reclaim_retry<T>(
        &mut self,
        mut operation: impl FnMut(&mut Self) -> Result<T, Error>,
    ) -> Result<T, Error> {
        let mut result = operation(self);
        for _ in 0..2 {
            let Err(Error::Resource(ResourceError::LimitExceeded {
                domain,
                resource: ResourceKind::KernelMemoryBytes,
                ..
            })) = &result
            else {
                break;
            };
            // Only unpublished preparation is retried. The worker scans the
            // denying domain (including descendant mount sponsors), never
            // waits for our state mutex, and drops cache owners outside its
            // locks. Keep admissions paused through local pruning and retry.
            let Some(pause) =
                crate::mm::reclaim::reclaim(crate::mm::reclaim::Target::Domain(*domain))
            else {
                break;
            };
            self.records.retain(|record| record.content.is_alive());
            result = operation(self);
            drop(pause);
        }
        result
    }

    fn scratch_charge(
        &mut self,
        domain: &ResourceDomain,
        bytes: usize,
    ) -> Result<CommittedCharge, Error> {
        self.reclaim_retry(|_| charge(domain, bytes))
    }

    fn prepare_node(
        &mut self,
        id: u64,
        path: String,
        domain: &ResourceDomain,
    ) -> Result<(FallibleArc<Node>, PreparedRecord), Error> {
        // Keep the path across a quota retry; all fallible quota admission and
        // object allocation precedes installing it in the prepared binding.
        let metadata_revision = self.metadata_revision;
        let (node, mut record) = self.reclaim_retry(|_| {
            let record_charge = record_charge(domain)?;
            let node = node(FileRecord::try_new(id, domain)?, domain)?;
            let record = Records::prepare(Record {
                id,
                metadata_revision,
                directory_revision: metadata_revision,
                path: String::new(),
                node: node.downgrade(),
                content: node.record.downgrade(),
                _charge: record_charge,
            })?;
            Ok((node, record))
        })?;
        record.set_path(path);
        Ok((node, record))
    }

    /// Directory retention follows its parent listing revision. State protects
    /// identity publication; backend operations release it while waiting for I/O.
    fn metadata_identity(&self, node: u64) -> Result<FileIdentity, Error> {
        let revision = self
            .records
            .get(node)
            .map_or(self.metadata_revision, |r| r.directory_revision);
        self.metadata_identity_at(node, revision)
    }
    fn entry_fallback_identity(&self) -> Result<FileIdentity, Error> {
        self.metadata_identity_at(0, self.metadata_revision)
    }
    fn metadata_identity_at(&self, node: u64, revision: u64) -> Result<FileIdentity, Error> {
        let node = node
            .checked_add(1)
            .and_then(NonZeroU64::new)
            .ok_or(Error::IdentifierExhausted)?;
        let revision = NonZeroU64::new(revision).ok_or(Error::IdentifierExhausted)?;
        Ok(FileIdentity::new(
            self.generation,
            NodeIdentity::new(node),
            ContentRevision::new(revision),
        ))
    }
    fn invalidate_entry(&mut self, node: u64) -> Result<(), Error> {
        self.writeback.changed = self.writeback.enabled();
        self.metadata_revision = self
            .metadata_revision
            .checked_add(1)
            .ok_or(Error::IdentifierExhausted)?;
        let revision = self.metadata_revision;
        let record = self.records.get_mut(node).ok_or(Error::Missing)?;
        record.metadata_revision = revision;
        self.invalidate_parent_listing(node, revision)
    }
    fn invalidate_parent_listing(&mut self, node: u64, revision: u64) -> Result<(), Error> {
        let record = self.records.get(node).ok_or(Error::Missing)?;
        let parent = record
            .path
            .rsplit_once('/')
            .map_or("", |(parent, _)| parent);
        let parent_id = self.records.find_path(parent).map(|r| r.id);
        if let Some(parent) = parent_id.and_then(|id| self.records.get_mut(id)) {
            parent.directory_revision = revision;
        }
        Ok(())
    }

    fn invalidate_metadata(&mut self) -> Result<(), Error> {
        self.writeback.changed = self.writeback.enabled();
        // Advance before issuing a potentially partially successful write.
        // Old pages remain reclaimable, but can no longer satisfy a lookup.
        self.metadata_revision = self
            .metadata_revision
            .checked_add(1)
            .ok_or(Error::IdentifierExhausted)?;
        let revision = self.metadata_revision;
        self.records.update_all_metadata(revision);
        Ok(())
    }

    fn path(&self, node: &Node) -> Result<String, Error> {
        copy(self.records.path(node.id())?)
    }
    fn lease(&mut self, path: String, domain: &ResourceDomain) -> Result<FallibleArc<Node>, Error> {
        // Bounded housekeeping avoids turning one open per directory entry
        // into a quadratic scan of the identity registry.
        for _ in 0..4 {
            drop(self.records.inspect_next());
        }
        let existing = self
            .records
            .find_path(&path)
            .map(|record| (record.node.upgrade(), record.content.upgrade()));
        if let Some((active, content)) = existing {
            if let Some(node) = active {
                return Ok(node);
            }
            if let Some(content) = content {
                let node = self.reclaim_retry(|_| node(content.clone(), domain))?;
                let Some(record) = self.records.get_mut(node.id()) else {
                    return Err(Error::InvalidBackendResult);
                };
                record.node = node.downgrade();
                return Ok(node);
            }
            // Reclaim may have dropped the final page pin since cleanup. No
            // active node survives without its record, so a fresh identity is
            // now safe. IDs are never reused within the mounted filesystem.
            if let Some(record) = self.records.find_path(&path) {
                let id = record.id;
                drop(self.records.remove(id));
            }
        }
        let id = self.next_id;
        let next = id.checked_add(1).ok_or(Error::IdentifierExhausted)?;
        let (node, record) = self.prepare_node(id, path, domain)?;
        self.records.insert(record);
        self.next_id = next;
        Ok(node)
    }
}
fn node(
    record: FallibleArc<FileRecord>,
    domain: &ResourceDomain,
) -> Result<FallibleArc<Node>, Error> {
    let charge = domain
        .reserve(ResourceAmount::ZERO.with(
            ResourceKind::KernelMemoryBytes,
            FallibleArc::<Node>::allocation_size() as u64,
        ))
        .map_err(Error::Resource)?
        .commit();
    // Reserve final storage before constructing lock and accounting state. A
    // lookup can be interrupted by IRQ-tail scheduling below this allocation;
    // it must not retain copies of the complete node through the allocator.
    let slot = hyper::mm::UniqueFallibleArc::try_new_uninit()?;
    Ok(slot
        .write(Node {
            mount_pins: AtomicU64::new(0),
            directory_revision: AtomicU64::new(0),
            locks: super::locks::FileLocks::new(),
            record,
            _charge: charge,
        })
        .into_shared())
}
fn child(parent: &str, name: Name<'_>) -> Result<String, Error> {
    join(
        parent,
        core::str::from_utf8(name.as_bytes()).map_err(|_| Error::InvalidInput)?,
    )
}
fn join(parent: &str, name: &str) -> Result<String, Error> {
    let len = parent
        .len()
        .checked_add(name.len())
        .and_then(|n| n.checked_add(usize::from(!parent.is_empty())))
        .filter(|n| *n <= MAX_PATH_BYTES)
        .ok_or(Error::InvalidInput)?;
    let mut output = String::new();
    output
        .try_reserve_exact(len)
        .map_err(|_| Error::Allocation)?;
    output.push_str(parent);
    if !parent.is_empty() {
        output.push('/');
    }
    output.push_str(name);
    Ok(output)
}
fn copy(value: &str) -> Result<String, Error> {
    let mut output = String::new();
    output
        .try_reserve_exact(value.len())
        .map_err(|_| Error::Allocation)?;
    output.push_str(value);
    Ok(output)
}
fn within(path: &str, parent: &str) -> bool {
    parent.is_empty()
        || path
            .strip_prefix(parent)
            .is_some_and(|tail| tail.starts_with('/'))
}
fn attributes(entry: &Entry) -> NodeAttributes {
    NodeAttributes::new(
        match entry.kind {
            hyper_filesystem::protocol::DIRECTORY => NodeKind::Directory,
            hyper_filesystem::protocol::SYMLINK => NodeKind::Symlink,
            _ => NodeKind::File,
        },
        entry.mode,
        entry.size,
    )
}
fn map(error: BackendError) -> Error {
    match error {
        BackendError::Allocation => Error::Allocation,
        BackendError::Missing => Error::Missing,
        BackendError::Exists => Error::AlreadyExists,
        BackendError::NotEmpty => Error::NotEmpty,
        BackendError::InvalidInput => Error::InvalidInput,
        BackendError::Unsupported => Error::Unsupported,
        other => Error::Remote(other),
    }
}

fn record_charge(domain: &ResourceDomain) -> Result<CommittedCharge, Error> {
    // Both Weak owners retain their complete shared allocations after the
    // strong payload charges retire. Cover them here as well; while live this
    // deliberately charges conservatively rather than leaving dead headers
    // outside the mount sponsor's quota.
    let bytes = MAX_PATH_BYTES
        + Records::allocation_size()
        + FallibleArc::<Node>::allocation_size()
        + FallibleArc::<FileRecord>::allocation_size();
    domain
        .reserve(ResourceAmount::ZERO.with(ResourceKind::KernelMemoryBytes, bytes as u64))
        .map(|charge| charge.commit())
        .map_err(Error::Resource)
}

fn charge(domain: &ResourceDomain, bytes: usize) -> Result<CommittedCharge, Error> {
    domain
        .reserve(ResourceAmount::ZERO.with(ResourceKind::KernelMemoryBytes, bytes as u64))
        .map(|charge| charge.commit())
        .map_err(Error::Resource)
}

// Allocate one charged metadata buffer per mounted volume, under its existing
// sleepable lock. Reusing it removes per-call stack copies without per-I/O
// allocation. Keep construction out of the mount parser's live frame.
#[inline(never)]
fn metadata_scratch() -> Result<Box<Entry>, Error> {
    hyper::mm::try_box(Entry::empty()).map_err(|_| Error::Allocation)
}

fn to_kernel_time(value: hyper_filesystem::Timestamp) -> Option<crate::time::queue::Timestamp> {
    crate::time::queue::Timestamp::new(value.seconds(), value.nanoseconds())
}
fn to_service_time(value: crate::time::queue::Timestamp) -> hyper_filesystem::Timestamp {
    // Both timestamp types validate the same nanosecond range.
    match hyper_filesystem::Timestamp::new(value.seconds(), value.nanoseconds()) {
        Some(value) => value,
        None => hyper::debug::invariant_failure("invalid validated timestamp"),
    }
}
