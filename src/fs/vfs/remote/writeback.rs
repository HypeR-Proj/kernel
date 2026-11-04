// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Volume-ordered writeback of the common VFS file pages. The volume mutex
//! serializes publication, submission, namespace changes, and checkpoint fences.
//! Dirty owners retain content identity, never a node or mount lease.

use super::super::write_source::WriteSource;
use super::*;
use crate::fs::cache::{CacheKey, DirtyPage, FilePageIndex};

const PAGE: usize = hyper::mm::PAGE_SIZE as usize;
const MAX_DIRTY_PAGES: usize = 2048;
const BATCH_BYTES: usize = hyper_filesystem::protocol::DATA_BYTES;

struct Dirty {
    node: u64,
    offset: u64,
    page: DirtyPage<CachePage>,
    modified: Option<hyper_filesystem::Timestamp>,
}

pub(super) struct Writeback {
    pages: Vec<Dirty>,
    scratch: ScratchVec<u8>,
    interval: u64,
    deadline: u64,
    failure: Option<Error>,
    pub(super) changed: bool,
    _charge: Option<CommittedCharge>,
}
impl Writeback {
    pub(super) fn new(interval: u64, domain: &ResourceDomain) -> Result<Self, Error> {
        let capacity = if interval == 0 { 0 } else { MAX_DIRTY_PAGES };
        let charge = if capacity == 0 {
            None
        } else {
            Some(charge(domain, capacity * core::mem::size_of::<Dirty>())?)
        };
        let mut pages = Vec::new();
        pages
            .try_reserve_exact(capacity)
            .map_err(|_| Error::Allocation)?;
        let mut scratch = ScratchVec::new(ScratchBudget::new(domain));
        if interval != 0 {
            scratch.resize(BATCH_BYTES, 0)?;
        }
        let now = crate::time::monotonic_nanoseconds().map_err(|_| Error::InvalidInput)?;
        Ok(Self {
            pages,
            scratch,
            interval,
            deadline: now.saturating_add(interval),
            failure: None,
            changed: false,
            _charge: charge,
        })
    }
    pub(super) fn enabled(&self) -> bool {
        self.interval != 0
    }
    pub(super) fn modified(&self, node: u64) -> Option<hyper_filesystem::Timestamp> {
        self.pages
            .iter()
            .filter(|p| p.node == node)
            .filter_map(|p| p.modified)
            .max_by_key(|time| (time.seconds(), time.nanoseconds()))
    }
    pub(super) fn overlay(&self, node: u64, entry: &mut Entry) {
        entry.size = self.logical_length(node, entry.size);
        if let Some(modified) = self.modified(node) {
            entry.modified = Some(modified);
        }
    }
    pub(super) fn logical_length(&self, node: u64, size: u64) -> u64 {
        self.pages
            .iter()
            .filter(|p| p.node == node)
            .fold(size, |size, p| {
                size.max(p.offset + p.page.value().bytes().len() as u64)
            })
    }
}

struct Worker {
    filesystem: WeakFallibleArc<RemoteFs>,
    _charge: CommittedCharge,
}

extern "C" fn worker(address: usize) {
    // SAFETY: start_writeback transfers this unique initialized Box to exactly
    // one dormant kernel thread. No other owner accesses it after publication.
    let worker =
        unsafe { Box::from_raw(core::ptr::with_exposed_provenance_mut::<Worker>(address)) };
    loop {
        let Some(filesystem) = worker.filesystem.upgrade() else {
            return;
        };
        if filesystem.read_status().is_err() {
            return;
        }
        if let Ok(now) = crate::time::monotonic_nanoseconds()
            && let Err(error) = filesystem.periodic_writeback(now)
        {
            crate::pr_err!("HypeR: filesystem writeback stopped: {error:?}");
            return;
        }
        drop(filesystem);
        if let Err(error) = crate::task::sleep_s(1) {
            crate::diagnostics::crash::fatal(format_args!(
                "HypeR: writeback sleep failed: {error:?}"
            ));
        }
    }
}

impl RemoteFs {
    pub(in crate::fs::vfs) fn start_writeback(owner: &FallibleArc<Self>) -> Result<(), Error> {
        if !owner.state.lock().map_err(Error::Lock)?.writeback.enabled() {
            return Ok(());
        }
        let context = hyper::mm::try_box(Worker {
            filesystem: owner.downgrade(),
            _charge: charge(
                &owner.domain,
                core::mem::size_of::<Worker>() + FallibleArc::<Self>::allocation_size(),
            )?,
        })?;
        let address = Box::into_raw(context) as usize;
        let thread = match crate::task::scheduler::kthread_create("kfs-writeback", worker, address)
        {
            Ok(thread) => thread,
            Err(_) => {
                // SAFETY: Creation failed before a thread could own this Box.
                drop(unsafe {
                    Box::from_raw(core::ptr::with_exposed_provenance_mut::<Worker>(address))
                });
                return Err(Error::Allocation);
            }
        };
        if !matches!(crate::task::scheduler::thread_ready(thread), Ok(true)) {
            crate::diagnostics::crash::fatal(format_args!(
                "HypeR: writeback worker publication failed"
            ));
        }
        Ok(())
    }

    pub(in crate::fs::vfs) fn flush_writeback(&self) -> Result<(), Error> {
        self.state.lock().map_err(Error::Lock)?.flush_writeback()
    }
    fn periodic_writeback(&self, now: u64) -> Result<(), Error> {
        let mut state = self.state.lock().map_err(Error::Lock)?;
        state.check_writeback()?;
        if !state.writeback.enabled() {
            return Ok(());
        }
        let due = now >= state.writeback.deadline;
        let pressured =
            !state.writeback.pages.is_empty() && !crate::mm::cache_memory::admission_allowed();
        if !due && !pressured {
            return Ok(());
        }
        let result = if due && state.writeback.changed {
            state.sync_writeback()
        } else if pressured {
            state.flush_writeback()
        } else {
            Ok(())
        };
        if let Err(error) = result {
            state.writeback.failure = Some(error);
            return Err(error);
        }
        if due {
            // Coalesce ticks elapsed while waiting for the volume and device.
            let now = crate::time::monotonic_nanoseconds().unwrap_or(now);
            let interval = state.writeback.interval;
            let ticks = (now - state.writeback.deadline) / interval + 1;
            state.writeback.deadline = state
                .writeback
                .deadline
                .saturating_add(ticks.saturating_mul(interval));
        }
        Ok(())
    }
}

impl State {
    pub(super) fn check_writeback(&self) -> Result<(), Error> {
        self.volume.health.check().map_err(map)?;
        self.writeback.failure.map_or(Ok(()), Err)
    }
    pub(super) fn cache_write(
        &mut self,
        node: &Node,
        path: &str,
        identity: FileIdentity,
        length: u64,
        offset: u64,
        input: &WriteSource<'_>,
    ) -> Result<Option<usize>, Error> {
        let mut done = 0;
        while done < input.len() {
            let at = offset + done as u64;
            let base = at / PAGE as u64 * PAGE as u64;
            let within = (at - base) as usize;
            let count = (PAGE - within).min(input.len() - done);
            let new_length = length.max(at + count as u64);
            let valid = (new_length - base).min(PAGE as u64) as usize;
            let step = (|| -> Result<bool, Error> {
                if self.writeback.pages.len() == MAX_DIRTY_PAGES
                    && self
                        .writeback
                        .pages
                        .binary_search_by_key(&(node.id(), base), |p| (p.node, p.offset))
                        .is_err()
                {
                    self.flush_writeback()?;
                }
                let key = CacheKey::new(identity, FilePageIndex::new(base / PAGE as u64));
                let page = match self
                    .cache
                    .write_page(key, valid, &node.record, |old, page| {
                        let bytes = page.bytes_mut();
                        // Recycled storage is initialized, but may contain another
                        // file. Explicitly zero newly exposed EOF/hole bytes.
                        bytes.fill(0);
                        if within != 0 || count != valid {
                            if let Some(old) = old {
                                let n = old.bytes().len().min(bytes.len());
                                bytes[..n].copy_from_slice(&old.bytes()[..n]);
                            } else {
                                let existing =
                                    length.saturating_sub(base).min(valid as u64) as usize;
                                if existing != 0 {
                                    let read = self
                                        .volume
                                        .read_at(path, base, &mut bytes[..existing])
                                        .map_err(map)?;
                                    if read != existing {
                                        return Err(Error::InvalidBackendResult);
                                    }
                                }
                            }
                        }
                        input.copy_range(done, &mut bytes[within..within + count])
                    }) {
                    // Admission and allocation are separate fallible steps:
                    // concurrent demand can consume the watermark headroom.
                    // Nothing was published on error. Drain earlier accepted
                    // pages before the caller falls back to direct I/O.
                    Err(Error::Allocation) => return Ok(false),
                    result => result?,
                };
                let Some(page) = page else {
                    return Ok(false);
                };
                let dirty = Dirty {
                    node: node.id(),
                    offset: base,
                    page,
                    modified: crate::time::realtime().map(to_service_time),
                };
                match self
                    .writeback
                    .pages
                    .binary_search_by_key(&(node.id(), base), |p| (p.node, p.offset))
                {
                    Ok(index) => self.writeback.pages[index] = dirty,
                    Err(index) => self.writeback.pages.insert(index, dirty),
                }
                self.writeback.changed = true;
                Ok(true)
            })();
            match step {
                Ok(true) => done += count,
                Ok(false) => return Ok((done != 0).then_some(done)),
                Err(_) if done != 0 => return Ok(Some(done)),
                Err(error) => return Err(error),
            }
        }
        Ok(Some(done))
    }
    pub(super) fn flush_writeback(&mut self) -> Result<(), Error> {
        self.check_writeback()?;
        if self.writeback.pages.is_empty() {
            return Ok(());
        }
        let result = self.submit_writeback();
        if let Err(error) = result {
            self.writeback.failure = Some(error);
        }
        result
    }
    fn submit_writeback(&mut self) -> Result<(), Error> {
        self.invalidate_metadata()?;
        let mut first = 0;
        while first < self.writeback.pages.len() {
            let start = &self.writeback.pages[first];
            let (node, offset) = (start.node, start.offset);
            let mut length = 0;
            let mut end = first;
            while let Some(page) = self.writeback.pages.get(end) {
                let bytes = page.page.value().bytes();
                if page.node != node
                    || page.offset != offset + length as u64
                    || length + bytes.len() > BATCH_BYTES
                {
                    break;
                }
                self.writeback.scratch[length..length + bytes.len()].copy_from_slice(bytes);
                length += bytes.len();
                end += 1;
            }
            let actual = self
                .volume
                .write_at(
                    self.records.path(node)?,
                    offset,
                    &WriteSource::Bytes(&self.writeback.scratch[..length]),
                )
                .map_err(map)?;
            if actual != length {
                return Err(Error::Remote(BackendError::NoSpace));
            }
            if self
                .writeback
                .pages
                .get(end)
                .is_none_or(|next| next.node != node)
                && let Some(modified) = self.writeback.modified(node)
            {
                self.volume
                    .set_times(self.records.path(node)?, None, Some(modified))
                    .map_err(map)?;
            }
            first = end;
        }
        // Every submitted version has been accepted. Releasing dirty pins keeps
        // those same pages available for clean reads and ordinary reclamation.
        self.writeback.pages.clear();
        Ok(())
    }
    pub(super) fn sync_writeback(&mut self) -> Result<(), Error> {
        self.flush_writeback()?;
        if let Err(error) = self.volume.sync().map_err(map) {
            self.writeback.failure = Some(error);
            return Err(error);
        }
        self.writeback.changed = false;
        Ok(())
    }
}
