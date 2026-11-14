// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Canonical mutable file pages and their bounded, ordered snapshot coordinator.
//!
//! The first implementation reserves at most 256 resident frames per mount.
//! Each admitted frame includes dirty ownership: a fault or a pending IPC does
//! not reserve additional global dirty credits. One private snapshot and a
//! preallocated finite-cut vector guarantee capture progress without allocation.

use super::{
    file_record::FileRecord,
    instance::{Error, Location},
    remote::RemoteFs,
};
use crate::accounting::{CommittedCharge, ResourceAmount, ResourceDomain, ResourceKind};
use crate::sync::Mutex;
use alloc::{boxed::Box, vec::Vec};
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use hyper::mm::{FallibleArc, WeakFallibleArc};
use hyper::sync::InterruptSpinLock;

mod adapter;
mod atomic;
mod page;
mod state;
#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
pub(crate) use atomic::FileFailureTestControl;
pub(crate) use atomic::{FileAtomicWord, PreparedFileFailure};
pub(crate) use page::{SharedFilePage, SharedInstall, SharedMutation, SharedPin};
const PAGE: usize = hyper::mm::PAGE_SIZE as usize;
const MAX_PAGES: usize = 256;
struct ResidentBudget {
    used: AtomicUsize,
}
struct FramePermit(FallibleArc<ResidentBudget>);
impl Drop for FramePermit {
    fn drop(&mut self) {
        self.0.used.fetch_sub(1, Ordering::Release);
    }
}
impl ResidentBudget {
    fn reserve(owner: &FallibleArc<Self>) -> Result<FramePermit, Error> {
        owner
            .used
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                (used < MAX_PAGES).then_some(used + 1)
            })
            .map_err(|_| Error::Allocation)?;
        Ok(FramePermit(owner.clone()))
    }
}

fn charge(domain: &ResourceDomain, bytes: usize) -> Result<CommittedCharge, Error> {
    Ok(domain
        .reserve(ResourceAmount::ZERO.with(ResourceKind::KernelMemoryBytes, bytes as u64))
        .map_err(Error::Resource)?
        .commit())
}
fn vfs_error(error: super::Error) -> Error {
    match error {
        super::Error::Backend(error) => error,
        super::Error::Allocation => Error::Allocation,
        super::Error::Busy => Error::Busy,
        super::Error::Unsupported => Error::Unsupported,
        _ => Error::InvalidInput,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SharedFileIdentity {
    pub filesystem: u64,
    pub node: u64,
}

pub(super) struct SharedFile {
    filesystem: WeakFallibleArc<RemoteFs>,
    record: WeakFallibleArc<FileRecord>,
    identity: SharedFileIdentity,
    size: AtomicU64,
    mappings: AtomicUsize,
    writable_mappings: AtomicUsize,
    sync_bindings:
        InterruptSpinLock<crate::mm::user_space::SyncBindings, crate::hal::irq::LocalMask>,
    _charge: CommittedCharge,
}
impl SharedFile {
    pub(super) fn writable_mapped(&self) -> bool {
        self.writable_mappings.load(Ordering::Acquire) != 0
    }
    pub(super) fn mapped(&self) -> bool {
        self.mappings.load(Ordering::Acquire) != 0
    }
    pub(super) fn page(owner: &FallibleArc<Self>, offset: u64) -> Result<SharedFilePage, Error> {
        owner
            .filesystem
            .upgrade()
            .ok_or(Error::Missing)?
            .shared_page(owner, offset)
    }
    pub(super) fn set_length(&self, length: u64) {
        self.size.store(length, Ordering::Release);
    }
    pub(super) fn read(
        owner: &FallibleArc<Self>,
        offset: u64,
        output: &mut [u8],
    ) -> Result<usize, Error> {
        let length = output
            .len()
            .min(owner.size.load(Ordering::Acquire).saturating_sub(offset) as usize);
        let mut done = 0;
        while done < length {
            let at = offset + done as u64;
            let base = at / PAGE as u64 * PAGE as u64;
            let within = (at - base) as usize;
            let count = (length - done).min(PAGE - within);
            match Self::page(owner, base) {
                Ok(page) => page
                    .read_exposed(within, &mut output[done..done + count])
                    .map_err(vfs_error)?,
                Err(Error::Busy | Error::Allocation) => {
                    // No canonical frame exists for this range. A private
                    // bounded backend read is coherent and creates no alias.
                    let filesystem = owner.filesystem.upgrade().ok_or(Error::Missing)?;
                    let actual = filesystem.shared_read(
                        owner.identity.node,
                        at,
                        &mut output[done..done + count],
                    )?;
                    done += actual;
                    if actual != count {
                        break;
                    }
                    continue;
                }
                Err(_) if done != 0 => break,
                Err(error) => return Err(error),
            }
            done += count;
        }
        Ok(done)
    }
    /// All input bytes are staged before mutation, so mapped source/destination
    /// overlap cannot overwrite a later unread source fragment.
    pub(super) fn write(
        owner: &FallibleArc<Self>,
        offset: u64,
        input: &super::write_source::WriteSource<'_>,
    ) -> Result<(usize, u64), Error> {
        let end = offset
            .checked_add(input.len() as u64)
            .ok_or(Error::InvalidSize)?;
        if end > owner.size.load(Ordering::Acquire) {
            return Err(Error::Busy);
        }
        let filesystem = owner.filesystem.upgrade().ok_or(Error::Missing)?;
        let mut staged =
            super::scratch::ScratchVec::new(super::scratch::ScratchBudget::new(&filesystem.domain));
        super::scratch::resize_bytes(&mut staged, input.len())?;
        input.copy_to(&mut staged)?;
        let mut done = 0;
        while done < staged.len() {
            let at = offset + done as u64;
            let base = at / PAGE as u64 * PAGE as u64;
            let within = (at - base) as usize;
            let count = (staged.len() - done).min(PAGE - within);
            let page = match Self::page(owner, base) {
                Ok(page) => page,
                Err(_) if done != 0 => break,
                Err(error) => return Err(error),
            };
            let mutation = match page.try_begin_write() {
                Ok(guard) => guard,
                Err(_) if done != 0 => break,
                Err(error) => return Err(vfs_error(error)),
            };
            page.write_exposed(&mutation, within, &staged[done..done + count])
                .map_err(vfs_error)?;
            done += count;
        }
        Ok((done, offset + done as u64))
    }
}

struct SourceLease {
    location: InterruptSpinLock<Option<Location>, crate::hal::irq::LocalMask>,
    file: FallibleArc<SharedFile>,
    writable: bool,
    sponsor: ResourceDomain,
    _charge: CommittedCharge,
}
impl SourceLease {
    fn retire(&self) {
        let location = self.location.with(Option::take);
        if location.is_some() {
            if self.writable {
                self.file.writable_mappings.fetch_sub(1, Ordering::Release);
            }
            self.file.mappings.fetch_sub(1, Ordering::Release);
        }
        drop(location);
    }
}
impl Drop for SourceLease {
    fn drop(&mut self) {
        self.retire();
    }
}

#[derive(Clone)]
pub(crate) struct SharedFileSource {
    lease: FallibleArc<SourceLease>,
}
impl SharedFileSource {
    pub(super) fn acquire(
        location: Location,
        offset: u64,
        length: u64,
        writable: bool,
        sponsor: &ResourceDomain,
    ) -> Result<Self, super::Error> {
        let charge = charge(sponsor, FallibleArc::<SourceLease>::allocation_size())?;
        let file = location
            .mount()
            .filesystem()
            .promote_shared(location.node())?;
        let mut content = location.node().content().lock()?;
        let filesystem = location.mount().filesystem();
        filesystem.read_status()?;
        content.length(|| {
            filesystem
                .attributes(location.node())
                .map(|attributes| attributes.size())
        })?;
        if writable {
            content.check_writable()?;
        }
        let size = file.size.load(Ordering::Acquire);
        if length == 0
            || !offset.is_multiple_of(PAGE as u64)
            || !length.is_multiple_of(PAGE as u64)
            || offset.checked_add(length).is_none_or(|end| end > size)
        {
            return Err(Error::InvalidInput.into());
        }
        if writable {
            file.writable_mappings
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                    count.checked_add(1)
                })
                .map_err(|_| Error::IdentifierExhausted)?;
        }
        if file
            .mappings
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                count.checked_add(1)
            })
            .is_err()
        {
            if writable {
                file.writable_mappings.fetch_sub(1, Ordering::Release);
            }
            return Err(Error::IdentifierExhausted.into());
        }
        drop(content);
        Ok(Self {
            lease: FallibleArc::try_new(SourceLease {
                location: InterruptSpinLock::new(Some(location)),
                file,
                writable,
                sponsor: sponsor.clone(),
                _charge: charge,
            })
            .map_err(|_| Error::Allocation)?,
        })
    }
    pub(crate) fn identity(&self) -> SharedFileIdentity {
        self.lease.file.identity
    }
    pub(crate) fn size(&self) -> u64 {
        self.lease.file.size.load(Ordering::Acquire)
    }
    pub(crate) fn retire(&self) {
        self.lease.retire();
    }
    pub(crate) fn page(&self, offset: u64) -> Result<SharedFilePage, super::Error> {
        // A late fault operation is an active namespace lease until its fill
        // finishes, even if concurrent unmap retires the mapping lineage.
        let _location = self
            .lease
            .location
            .with(|location| location.clone())
            .ok_or(super::Error::Busy)?;
        if !offset.is_multiple_of(PAGE as u64)
            || offset
                .checked_add(PAGE as u64)
                .is_none_or(|end| end > self.size())
        {
            return Err(super::Error::InvalidInput);
        }
        // Closing the final mapping or resizing cannot race a late fill.
        let record = self.lease.file.record.upgrade().ok_or(Error::Missing)?;
        let content = record.content.lock()?;
        if !self.lease.location.with(|location| location.is_some()) {
            return Err(super::Error::Busy);
        }
        let result = SharedFile::page(&self.lease.file, offset);
        drop(content);
        if !matches!(result, Err(Error::Allocation)) {
            return result.map_err(Into::into);
        }
        let filesystem = self.lease.file.filesystem.upgrade().ok_or(Error::Missing)?;
        // The caller's old mapping snapshot may still own every retired frame.
        // Unwind its preparation before retrying so those physical permits can
        // be released. An immediate retry here would falsely report OOM.
        if filesystem.reclaim_shared()? {
            Err(super::Error::Busy)
        } else {
            result.map_err(Into::into)
        }
    }
    /// Independent operation authority, acquired before a copy can wait or a
    /// sync can outlive its mapping. Retirement of the original mapping cannot
    /// release this operation's extent or potential-write exclusion.
    pub(crate) fn retain_sync(&self) -> Result<Self, super::Error> {
        let charge = charge(
            &self.lease.sponsor,
            FallibleArc::<SourceLease>::allocation_size(),
        )?;
        let location = self
            .lease
            .location
            .with(|location| location.clone())
            .ok_or(super::Error::Busy)?;
        let content = location.node().content().lock()?;
        if !self.lease.location.with(|location| location.is_some()) {
            return Err(super::Error::Busy);
        }
        location.mount().filesystem().read_status()?;
        let file = &self.lease.file;
        if self.lease.writable {
            content.check_writable()?;
            file.writable_mappings
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                    count.checked_add(1)
                })
                .map_err(|_| Error::IdentifierExhausted)?;
        }
        if file
            .mappings
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                count.checked_add(1)
            })
            .is_err()
        {
            if self.lease.writable {
                file.writable_mappings.fetch_sub(1, Ordering::Release);
            }
            return Err(Error::IdentifierExhausted.into());
        }
        drop(content);
        // try_new drops the unpublished SourceLease on failure. Its retirement
        // rolls both counters back while the original mapping stays untouched.
        Ok(Self {
            lease: FallibleArc::try_new(SourceLease {
                location: InterruptSpinLock::new(Some(location)),
                file: file.clone(),
                writable: self.lease.writable,
                sponsor: self.lease.sponsor.clone(),
                _charge: charge,
            })
            .map_err(|_| Error::Allocation)?,
        })
    }
    pub(crate) fn sync(&self, scope: u32) -> Result<(), super::Error> {
        let location = self
            .lease
            .location
            .with(|location| location.clone())
            .ok_or(super::Error::Busy)?;
        location
            .mount()
            .filesystem()
            .sync(location.node(), scope as u64)
            .map_err(Into::into)
    }
}

#[derive(Clone)]
struct Resident {
    file: FallibleArc<SharedFile>,
    offset: u64,
    page: SharedFilePage,
}
struct Registry {
    pages: Vec<Resident>,
    loading: usize,
}
struct Cut {
    resident: Resident,
    epoch: u64,
}
struct Snapshot {
    bytes: Box<[u8; PAGE]>,
    cut: Vec<Cut>,
}
/// Always release observation owners, including after partial/cancelled work.
/// Private snapshot bytes stay mount-owned, but old cut references must not
/// consume physical residency permits until an unrelated later flush.
struct SnapshotSession<'a>(crate::sync::MutexGuard<'a, Snapshot>);
impl core::ops::Deref for SnapshotSession<'_> {
    type Target = Snapshot;
    fn deref(&self) -> &Snapshot {
        &self.0
    }
}
impl core::ops::DerefMut for SnapshotSession<'_> {
    fn deref_mut(&mut self) -> &mut Snapshot {
        &mut self.0
    }
}
impl Drop for SnapshotSession<'_> {
    fn drop(&mut self) {
        self.0.cut.clear();
    }
}

pub(super) struct SharedCache {
    registry: Mutex<Registry>,
    budget: FallibleArc<ResidentBudget>,
    snapshot: Mutex<Snapshot>,
    _charge: CommittedCharge,
}
impl SharedCache {
    pub(super) fn new(domain: &ResourceDomain) -> Result<Self, Error> {
        let charge = charge(
            domain,
            PAGE + FallibleArc::<ResidentBudget>::allocation_size()
                + MAX_PAGES * (core::mem::size_of::<Resident>() + core::mem::size_of::<Cut>()),
        )?;
        let mut pages = Vec::new();
        let mut cut = Vec::new();
        pages
            .try_reserve_exact(MAX_PAGES)
            .map_err(|_| Error::Allocation)?;
        cut.try_reserve_exact(MAX_PAGES)
            .map_err(|_| Error::Allocation)?;
        let mut bytes = hyper::mm::try_box_uninit::<[u8; PAGE]>()?;
        // SAFETY: initialize the unique private snapshot in place, not on stack.
        let bytes = unsafe {
            bytes.as_mut_ptr().cast::<u8>().write_bytes(0, PAGE);
            bytes.assume_init()
        };
        Ok(Self {
            budget: FallibleArc::try_new(ResidentBudget {
                used: AtomicUsize::new(0),
            })?,
            registry: Mutex::new(Registry { pages, loading: 0 }),
            snapshot: Mutex::new(Snapshot { bytes, cut }),
            _charge: charge,
        })
    }
    pub(super) fn promote(
        &self,
        filesystem: &FallibleArc<RemoteFs>,
        record: &FallibleArc<FileRecord>,
        identity: SharedFileIdentity,
        size: u64,
    ) -> Result<FallibleArc<SharedFile>, Error> {
        let file = FallibleArc::try_new(SharedFile {
            filesystem: filesystem.downgrade(),
            record: record.downgrade(),
            identity,
            size: AtomicU64::new(size),
            mappings: AtomicUsize::new(0),
            writable_mappings: AtomicUsize::new(0),
            sync_bindings: InterruptSpinLock::new(crate::mm::user_space::SyncBindings::new()),
            _charge: charge(
                &filesystem.domain,
                FallibleArc::<SharedFile>::allocation_size(),
            )?,
        })?;
        Ok(file)
    }
    pub(super) fn page(
        &self,
        file: &FallibleArc<SharedFile>,
        offset: u64,
    ) -> Result<SharedFilePage, Error> {
        let owner = {
            let mut registry = self.registry.lock().map_err(Error::Lock)?;
            if let Some(resident) = registry.pages.iter().find(|resident| {
                resident.file.identity == file.identity && resident.offset == offset
            }) {
                return Ok(resident.page.clone());
            }
            if registry.pages.len() + registry.loading == MAX_PAGES {
                return Err(Error::Allocation);
            }
            registry.loading += 1;
            file.clone()
        };
        let loaded = ResidentBudget::reserve(&self.budget)
            .and_then(|permit| SharedFilePage::load(file, offset, permit));
        // Reservation cleanup is not cancellable after backend I/O.
        let mut registry = loop {
            match self.registry.lock() {
                Ok(registry) => break registry,
                Err(crate::sync::Error::WaitInterrupted(crate::task::WaitOutcome::Cancelled)) => {
                    continue;
                }
                Err(_) => hyper::debug::invariant_failure("shared page fill reservation lost"),
            }
        };
        registry.loading -= 1;
        let page = loaded?;
        if let Some(existing) = registry
            .pages
            .iter()
            .find(|resident| resident.file.identity == file.identity && resident.offset == offset)
        {
            let existing = existing.page.clone();
            drop(registry);
            drop(page);
            return Ok(existing);
        }
        registry.pages.push(Resident {
            file: owner,
            offset,
            page: page.clone(),
        });
        Ok(page)
    }
    /// Snapshot gate serializes finite cuts, not ordinary access or mutations.
    /// No registry/page/AS lock is held across a callback or backend operation.
    pub(super) fn flush(
        &self,
        filesystem: &RemoteFs,
        selected: Option<u64>,
        evict: bool,
    ) -> Result<bool, Error> {
        self.flush_with_policy(filesystem, selected, evict, false)
    }
    pub(super) fn reclaim(&self, filesystem: &RemoteFs) -> Result<bool, Error> {
        self.flush_with_policy(filesystem, None, true, true)
    }
    fn flush_with_policy(
        &self,
        filesystem: &RemoteFs,
        selected: Option<u64>,
        evict: bool,
        opportunistic: bool,
    ) -> Result<bool, Error> {
        let mut snapshot = SnapshotSession(self.snapshot.lock().map_err(Error::Lock)?);
        snapshot.cut.clear();
        {
            let registry = self.registry.lock().map_err(Error::Lock)?;
            for resident in &registry.pages {
                if selected.is_some_and(|node| node != resident.file.identity.node) {
                    continue;
                }
                if let Some(epoch) = resident.page.dirty_epoch() {
                    snapshot.cut.push(Cut {
                        resident: resident.clone(),
                        epoch,
                    });
                } else if evict {
                    snapshot.cut.push(Cut {
                        resident: resident.clone(),
                        epoch: resident.page.epoch(),
                    });
                }
            }
        }
        let changed = !snapshot.cut.is_empty();
        let mut pinned = false;
        let mut retired = 0usize;
        for index in 0..snapshot.cut.len() {
            let resident = snapshot.cut[index].resident.clone();
            let page = &resident.page;
            // An earlier cut cannot exist under this gate. A successor from a
            // separately retired owner is never selected by stale identity.
            if page.epoch() != snapshot.cut[index].epoch {
                continue;
            }
            let closed = if opportunistic {
                page.try_close_for_reclaim()
            } else {
                loop {
                    match if evict {
                        page.close_for_eviction()
                    } else {
                        Some(page.close())
                    } {
                        None => break false,
                        Some(true) => break true,
                        Some(false) => {
                            filesystem.read_status()?;
                            if crate::task::scheduler::yield_now().is_err() {
                                page.reopen();
                                return Err(Error::Busy);
                            }
                        }
                    }
                }
            };
            if !closed {
                pinned = true;
                continue;
            }
            if let Err(error) = page.withdraw(evict) {
                page.reopen();
                if opportunistic && error == Error::Busy {
                    pinned = true;
                    continue;
                }
                return Err(error);
            }
            let length = resident
                .file
                .size
                .load(Ordering::Acquire)
                .saturating_sub(resident.offset)
                .min(PAGE as u64) as usize;
            let dirty = page.dirty();
            if let Err(error) = page.capture(&mut snapshot.bytes[..], evict) {
                page.reopen();
                return Err(error);
            }
            if dirty
                && let Err(error) = filesystem.shared_write(
                    resident.file.identity.node,
                    resident.offset,
                    &snapshot.bytes[..length],
                )
            {
                page.submission_failed();
                return Err(error);
            }
            if evict {
                let mut registry = self.registry.lock().map_err(Error::Lock)?;
                registry
                    .pages
                    .retain(|entry| entry.page.identity() != page.identity());
                retired += 1;
            }
        }
        snapshot.cut.clear();
        if pinned && !opportunistic {
            return Err(Error::Busy);
        }
        Ok(if opportunistic { retired != 0 } else { changed })
    }
    /// Caller holds the file content gate, excluding every fill and ordinary
    /// mutation for this identity. A false result remains stable until that
    /// gate is released or the caller itself performs a fill.
    pub(super) fn has_resident(&self, node: u64) -> Result<bool, Error> {
        let registry = self.registry.lock().map_err(Error::Lock)?;
        Ok(registry
            .pages
            .iter()
            .any(|resident| resident.file.identity.node == node))
    }
    pub(super) fn needs_capacity(&self, pages: usize) -> bool {
        self.registry
            .try_lock()
            .ok()
            .flatten()
            .is_none_or(|registry| registry.pages.len() + registry.loading + pages > MAX_PAGES)
    }
    pub(super) fn idle(&self) -> bool {
        self.registry
            .try_lock()
            .ok()
            .flatten()
            .is_some_and(|registry| {
                registry
                    .pages
                    .iter()
                    .any(|resident| !resident.file.mapped())
            })
    }
    pub(super) fn pressured(&self) -> bool {
        self.registry
            .try_lock()
            .ok()
            .flatten()
            .is_some_and(|registry| registry.pages.len() >= MAX_PAGES * 3 / 4)
    }
    pub(super) fn fail(&self) -> Result<u64, Error> {
        let mut snapshot = SnapshotSession(self.snapshot.lock().map_err(Error::Lock)?);
        snapshot.cut.clear();
        {
            let registry = self.registry.lock().map_err(Error::Lock)?;
            for resident in &registry.pages {
                snapshot.cut.push(Cut {
                    resident: resident.clone(),
                    epoch: 0,
                });
            }
        }
        let mut discarded = 0_u64;
        for entry in &snapshot.cut {
            while !entry.resident.page.close() {
                if crate::task::scheduler::yield_now().is_err() {
                    return Err(Error::Busy);
                }
            }
            entry.resident.page.withdraw(true)?;
            entry.resident.page.fail();
            if entry.resident.page.dirty() {
                discarded = discarded.saturating_add(
                    entry
                        .resident
                        .file
                        .size
                        .load(Ordering::Acquire)
                        .saturating_sub(entry.resident.offset)
                        .min(PAGE as u64),
                );
            }
        }
        snapshot.cut.clear();
        Ok(discarded)
    }
}
