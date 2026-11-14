// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Exact dirty versions survive preparation, submission and device completion.
//! Pending cache publication is independent of ordered backend work. A sync cut
//! moves its finite owner set to frozen storage; later writes cannot extend it.

use super::super::write_source::WriteSource;
use super::*;
use crate::fs::cache::{CacheKey, DirtyPage, FilePageIndex};
use crate::object::{SignalMask, SignalSource, SignalState, SignalWaitRequest, wait_many};
const PAGE: usize = hyper::mm::PAGE_SIZE as usize;
const MAX_DIRTY_PAGES: usize = 2048;
const BACKGROUND_HIGH_PAGES: usize = 1536;
const BACKGROUND_LOW_PAGES: usize = 1024;
const BATCH_BYTES: usize = hyper_filesystem::protocol::DATA_BYTES;
const BATCH_PAGES: usize = BATCH_BYTES / PAGE;
const BATCHES: usize = hyper_filesystem::protocol::MAX_WRITEBACK_BATCHES;
// Release backend ownership after a bounded pressure burst; explicit sync instead
// drains its finite frozen cut. Rolling reuse must not monopolize cold operations.
const PRESSURE_BATCHES: usize = BATCHES * 2;
mod order;
mod queue;
type Dirty = queue::Pending<DirtyPage<CachePage>>;
struct Wake {
    signal: SignalState,
    _charge: CommittedCharge,
}
impl Wake {
    fn update(&self, ready: bool) {
        let bit = SignalMask::from_trusted_bits(1);
        let (clear, set) = if ready {
            (SignalMask::EMPTY, bit)
        } else {
            (bit, SignalMask::EMPTY)
        };
        if self.signal.update(clear, set).is_err() {
            hyper::debug::invariant_failure("filesystem writeback wake failed");
        }
    }
}
#[derive(Clone, Copy)]
struct Flight {
    node: u64,
    end: u64,
    modified: Option<hyper_filesystem::Timestamp>,
    pages: usize,
    bytes: usize,
}
pub(super) struct Writeback {
    pages: Vec<Dirty>,
    frozen: Vec<Dirty>,
    flights: [Option<Flight>; BATCHES],
    interval: u64,
    deadline: u64,
    failure: Option<Error>,
    wake: Option<FallibleArc<Wake>>,
    background: bool,
    retired: bool,
    cursor: u64,
    version: u64,
    pub(super) changed: bool,
    pub(super) fenced: bool,
    _charge: Option<CommittedCharge>,
}
impl Writeback {
    pub(super) fn new(interval: u64, domain: &ResourceDomain) -> Result<Self, Error> {
        let capacity = if interval == 0 { 0 } else { MAX_DIRTY_PAGES };
        let charge = if capacity == 0 {
            None
        } else {
            Some(charge(
                domain,
                capacity * core::mem::size_of::<Dirty>() * 2,
            )?)
        };
        let mut pages = Vec::new();
        let mut frozen = Vec::new();
        pages
            .try_reserve_exact(capacity)
            .map_err(|_| Error::Allocation)?;
        frozen
            .try_reserve_exact(capacity)
            .map_err(|_| Error::Allocation)?;
        let now = crate::time::monotonic_nanoseconds().map_err(|_| Error::InvalidInput)?;
        let wake = if interval == 0 {
            None
        } else {
            Some(FallibleArc::try_new(Wake {
                signal: SignalState::new(),
                _charge: super::charge(domain, FallibleArc::<Wake>::allocation_size())?,
            })?)
        };
        Ok(Self {
            pages,
            frozen,
            flights: [None; BATCHES],
            interval,
            deadline: now.saturating_add(interval),
            failure: None,
            wake,
            background: false,
            retired: false,
            cursor: 0,
            version: 0,
            changed: false,
            fenced: false,
            _charge: charge,
        })
    }
    pub(super) fn enabled(&self) -> bool {
        self.interval != 0
    }
    fn count(&self) -> usize {
        self.pages.len()
            + self.frozen.len()
            + self
                .flights
                .iter()
                .flatten()
                .map(|f| f.pages)
                .sum::<usize>()
    }
    fn notify_pressure(&mut self) {
        if self.count() >= BACKGROUND_HIGH_PAGES && !self.background {
            self.background = true;
            if let Some(wake) = &self.wake {
                wake.update(true);
            }
        }
    }
    pub(super) fn modified(&self, node: u64) -> Option<hyper_filesystem::Timestamp> {
        self.pages
            .iter()
            .chain(&self.frozen)
            .filter(|p| p.node == node)
            .filter_map(|p| p.modified)
            .chain(
                self.flights
                    .iter()
                    .flatten()
                    .filter(|f| f.node == node)
                    .filter_map(|f| f.modified),
            )
            .max_by_key(|t| (t.seconds(), t.nanoseconds()))
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
            .chain(&self.frozen)
            .filter(|p| p.node == node)
            .map(|p| p.offset + p.page.value().bytes().len() as u64)
            .chain(
                self.flights
                    .iter()
                    .flatten()
                    .filter(|f| f.node == node)
                    .map(|f| f.end),
            )
            .fold(size, u64::max)
    }
    fn freeze(&mut self, selected: Option<u64>) {
        queue::freeze(&mut self.pages, &mut self.frozen, selected);
    }
}
struct Batch {
    pages: Vec<Dirty>,
    path: String,
    node: u64,
    offset: u64,
    length: usize,
    modified: Option<hyper_filesystem::Timestamp>,
    last: bool,
    progress: BatchProgress,
}
// A missing receipt is not evidence that the backend completed a write.
// In particular, lost acknowledgements and consumed failed receipts must never
// enter the synchronous-success retirement path on a later cleanup attempt.
#[derive(Clone, Copy)]
enum BatchProgress {
    Prepared,
    Submitted { token: u64, accepted: usize },
    Completed { accepted: usize },
    Failed(Error),
}
pub(super) struct Batches {
    slots: [Batch; BATCHES],
    order: order::Order<BATCHES>,
    _charge: Option<CommittedCharge>,
}
impl super::super::write_source::WriteFragments for Batch {
    fn count(&self) -> usize {
        self.pages.len()
    }
    fn fragment(&self, index: usize) -> Option<&[u8]> {
        self.pages.get(index).map(|page| page.page.value().bytes())
    }
}
impl Batches {
    pub(super) fn abandon(&mut self) {
        self.order = order::Order::new();
        for slot in &mut self.slots {
            slot.pages.clear();
            slot.progress = BatchProgress::Prepared;
        }
    }
    pub(super) fn new(domain: &ResourceDomain, enabled: bool) -> Result<Self, Error> {
        let capacity = if enabled { BATCH_PAGES } else { 0 };
        let path_capacity = if enabled { MAX_PATH_BYTES } else { 0 };
        let charge = if enabled {
            Some(charge(
                domain,
                BATCHES * (capacity * core::mem::size_of::<Dirty>() + path_capacity)
                    + if enabled { PAGE } else { 0 },
            )?)
        } else {
            None
        };
        let slot = || -> Result<Batch, Error> {
            let mut pages = Vec::new();
            pages
                .try_reserve_exact(capacity)
                .map_err(|_| Error::Allocation)?;
            let mut path = String::new();
            path.try_reserve_exact(path_capacity)
                .map_err(|_| Error::Allocation)?;
            Ok(Batch {
                pages,
                path,
                node: 0,
                offset: 0,
                length: 0,
                modified: None,
                last: false,
                progress: BatchProgress::Prepared,
            })
        };
        Ok(Self {
            slots: [slot()?, slot()?],
            order: order::Order::new(),
            _charge: charge,
        })
    }
}
struct Worker {
    filesystem: WeakFallibleArc<RemoteFs>,
    wake: FallibleArc<Wake>,
    domain: ResourceDomain,
    _charge: CommittedCharge,
}

extern "C" fn worker(address: usize) {
    // SAFETY: start_writeback transfers this unique initialized Box to exactly
    // one dormant kernel thread. No other owner accesses it after publication.
    let worker =
        unsafe { Box::from_raw(core::ptr::with_exposed_provenance_mut::<Worker>(address)) };
    let mut retirement_reported = false;
    loop {
        // Clear before inspecting durable dirty state. A crossing after the
        // inspection leaves the level asserted for the timed wait below.
        worker.wake.update(false);
        let Some(filesystem) = worker.filesystem.upgrade() else {
            return;
        };
        let mut retry_delay = false;
        let more = if filesystem.read_status().is_err() {
            match filesystem.revoke_failed_shared() {
                Ok(_) => return,
                Err(error) => {
                    if !retirement_reported {
                        crate::pr_crit!(
                            "HypeR: shared file alias retirement incomplete; retrying: {error:?}"
                        );
                        retirement_reported = true;
                    }
                    // Keep the owner observable and retry after a real delay;
                    // a crossing wake level must not turn failure into a spin.
                    retry_delay = true;
                    false
                }
            }
        } else {
            match crate::time::monotonic_nanoseconds() {
                Ok(now) => match filesystem.periodic_writeback(now) {
                    Ok(Some(more)) => more,
                    Ok(None) => return,
                    Err(Error::Busy) => true,
                    Err(Error::Allocation | Error::Resource(_) | Error::Lock(_))
                        if filesystem.read_status().is_ok() =>
                    {
                        // Only healthy local preparation may retry. RPC-stage
                        // failures latch health before returning, even when the
                        // provider reports an allocation-shaped status.
                        retry_delay = true;
                        false
                    }
                    Err(error) => {
                        filesystem.health.fail();
                        crate::pr_crit!(
                            "HypeR: filesystem writeback failed; mount failed: {error:?}"
                        );
                        // The next timed iteration retires all aliases; failed
                        // retirement itself remains retryable above.
                        retry_delay = true;
                        false
                    }
                },
                Err(_) => false,
            }
        };
        drop(filesystem);
        if retry_delay {
            if let Err(error) = crate::task::sleep_s(1) {
                crate::diagnostics::crash::fatal(format_args!(
                    "HypeR: writeback retry sleep failed: {error:?}"
                ));
            }
            continue;
        }
        if more {
            if crate::task::scheduler::cond_resched().is_err() {
                hyper::debug::invariant_failure("filesystem writeback reschedule failed");
            }
            continue;
        }
        // The event owns no filesystem lease. A finite fallback also observes
        // global cache pressure and destruction without requiring a drop RPC.
        if let Ok(now) = crate::time::monotonic_nanoseconds() {
            let requests = [SignalWaitRequest::new(
                SignalSource::new(&worker.wake.signal, SignalMask::from_trusted_bits(1)),
                1,
            )];
            if wait_many(
                &requests,
                &worker.domain,
                now.saturating_add(1_000_000_000),
                || false,
            )
            .is_ok()
            {
                continue;
            }
        }
        // Wait admission can fail under quota pressure. The original timer
        // path still guarantees progress without allocating a signal waiter.
        if let Err(error) = crate::task::sleep_s(1) {
            crate::diagnostics::crash::fatal(format_args!(
                "HypeR: writeback sleep failed: {error:?}"
            ));
        }
    }
}

impl RemoteFs {
    pub(in crate::fs::vfs) fn start_writeback(owner: &FallibleArc<Self>) -> Result<(), Error> {
        let Some(wake) = owner
            .state
            .lock()
            .map_err(Error::Lock)?
            .writeback
            .wake
            .clone()
        else {
            return Ok(());
        };
        let context = hyper::mm::try_box(Worker {
            filesystem: owner.downgrade(),
            wake,
            domain: owner.domain.clone(),
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
        self.session()?.flush_writeback()
    }
    fn periodic_writeback(&self, now: u64) -> Result<Option<bool>, Error> {
        let shared_due = {
            let state = self.state.lock().map_err(Error::Lock)?;
            if state.writeback.retired {
                return Ok(None);
            }
            now >= state.writeback.deadline
        };
        let shared_pressure = self
            .shared
            .as_ref()
            .is_some_and(|shared| shared.pressured() || shared.idle())
            || !crate::mm::cache_memory::admission_allowed();
        if shared_due {
            self.flush_shared(None, false)?;
        }
        if shared_pressure {
            self.reclaim_shared()?;
        }
        let mut state = self.session()?;
        // A worker can pass read_status before unmount, then wait behind its
        // backend gate. Successful publication retires it under that same gate;
        // revoked old capabilities are not a background writeback failure.
        if state.writeback.retired {
            return Ok(None);
        }
        state.check_writeback()?;
        if !state.writeback.enabled() {
            return Ok(Some(false));
        }
        let due = now >= state.writeback.deadline;
        let pressured =
            state.writeback.count() != 0 && !crate::mm::cache_memory::admission_allowed();
        if !due && !pressured && !state.writeback.background {
            return Ok(Some(false));
        }
        if due && state.writeback.changed {
            state.sync_writeback()?;
        } else if pressured || state.writeback.background {
            state.flush_batch(None)?;
        }
        if due {
            let now = crate::time::monotonic_nanoseconds().unwrap_or(now);
            let interval = state.writeback.interval;
            let ticks = now.saturating_sub(state.writeback.deadline) / interval + 1;
            state.writeback.deadline = state
                .writeback
                .deadline
                .saturating_add(ticks.saturating_mul(interval));
        }
        if state.writeback.count() <= BACKGROUND_LOW_PAGES {
            state.writeback.background = false;
        }
        Ok(Some(
            state.writeback.count() != 0
                && (state.writeback.background || !crate::mm::cache_memory::admission_allowed()),
        ))
    }
}
impl State {
    pub(super) fn retire_writeback(&mut self) {
        self.writeback.retired = true;
        if let Some(wake) = &self.writeback.wake {
            wake.update(true);
        }
    }
    pub(super) fn abandon_writeback(&mut self) -> u64 {
        let bytes = self
            .writeback
            .pages
            .iter()
            .chain(&self.writeback.frozen)
            .fold(0_u64, |n, p| {
                n.saturating_add(p.page.value().bytes().len() as u64)
            })
            .saturating_add(
                self.writeback
                    .flights
                    .iter()
                    .flatten()
                    .map(|f| f.bytes as u64)
                    .sum(),
            );
        self.writeback
            .failure
            .get_or_insert(Error::Remote(BackendError::Closed));
        self.writeback.pages.clear();
        self.writeback.frozen.clear();
        self.writeback.flights.fill(None);
        self.writeback.interval = 0;
        if let Some(wake) = &self.writeback.wake {
            wake.update(true);
        }
        bytes
    }
    pub(super) fn fail_writeback(&mut self, error: Error) -> Error {
        // No retry/recovery protocol exists for a failed dirty owner. Keep its
        // first cause and expose failed-detach even when the provider is alive
        // and its last reply was a nonterminal application-level rejection.
        self.health.fail();
        if self.writeback.failure.is_none() {
            self.writeback.failure = Some(error);
            crate::pr_crit!(
                "HypeR: filesystem writeback failed; mount failed: {:?}",
                error
            );
        }
        self.writeback.failure.unwrap_or(error)
    }
    pub(super) fn check_writeback(&self) -> Result<(), Error> {
        if let Some(error) = self.writeback.failure {
            return Err(error);
        }
        self.health.check().map_err(map)
    }
    /// Cache-only publication. A missing partial page or exhausted budget asks
    /// the caller to leave State before obtaining backend ownership.
    pub(super) fn cache_write(
        &mut self,
        node: &Node,
        identity: FileIdentity,
        length: u64,
        offset: u64,
        input: &WriteSource<'_>,
        fill: Option<&[u8]>,
    ) -> Result<Option<usize>, Error> {
        if self.writeback.fenced {
            return Ok(None);
        }
        let mut done = 0;
        while done < input.len() {
            let at = offset + done as u64;
            let base = at / PAGE as u64 * PAGE as u64;
            let within = (at - base) as usize;
            let count = (PAGE - within).min(input.len() - done);
            let valid = (length.max(at + count as u64) - base).min(PAGE as u64) as usize;
            let step = (|| -> Result<bool, Error> {
                let existing = self
                    .writeback
                    .pages
                    .binary_search_by_key(&(node.id(), base), |p| (p.node, p.offset));
                if self.writeback.count() >= MAX_DIRTY_PAGES && existing.is_err() {
                    return Ok(false);
                }
                let version = self
                    .writeback
                    .version
                    .checked_add(1)
                    .ok_or(Error::IdentifierExhausted)?;
                let key = CacheKey::new(identity, FilePageIndex::new(base / PAGE as u64));
                let partial = within != 0 || count != valid;
                if partial
                    && length > base
                    && self.cache.lookup_mapping(key)?.is_none()
                    && fill.is_none()
                {
                    return Ok(false);
                }
                let page = match self
                    .cache
                    .write_page(key, valid, &node.record, |old, page| {
                        let bytes = page.bytes_mut();
                        bytes.fill(0);
                        if partial {
                            if let Some(old) = old {
                                let n = old.bytes().len().min(bytes.len());
                                bytes[..n].copy_from_slice(&old.bytes()[..n]);
                            } else if let Some(fill) = fill {
                                let n = fill.len().min(bytes.len());
                                bytes[..n].copy_from_slice(&fill[..n]);
                            }
                        }
                        input.copy_range(done, &mut bytes[within..within + count])
                    }) {
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
                    version,
                };
                match existing {
                    Ok(i) => self.writeback.pages[i] = dirty,
                    Err(i) => self.writeback.pages.insert(i, dirty),
                }
                self.writeback.version = version;
                self.writeback.changed = true;
                self.writeback.notify_pressure();
                Ok(true)
            })();
            match step {
                Ok(true) => done += count,
                Ok(false) => return Ok((done != 0).then_some(done)),
                Err(_) if done != 0 => return Ok(Some(done)),
                Err(e) => return Err(e),
            }
            // A supplied partial-page image belongs only to the first page.
            if fill.is_some() {
                break;
            }
        }
        Ok(Some(done))
    }
}
impl Session<'_> {
    pub(super) fn cache_write_filled(
        &mut self,
        node: &Node,
        identity: FileIdentity,
        length: u64,
        offset: u64,
        input: &WriteSource<'_>,
    ) -> Result<Option<usize>, Error> {
        if self.writeback.count() >= MAX_DIRTY_PAGES {
            self.flush_batch(None)?;
        }
        if let Some(written) = self.cache_write(node, identity, length, offset, input, None)? {
            return Ok(Some(written));
        }
        if !crate::mm::cache_memory::admission_allowed() {
            return Ok(None);
        }
        let base = offset / PAGE as u64 * PAGE as u64;
        let count = (PAGE - (offset - base) as usize).min(input.len());
        let existing = length.saturating_sub(base).min(PAGE as u64) as usize;
        if existing == 0 {
            return Ok(None);
        }
        self.bind_path(node.id())?;
        self.finish_batches()?;
        self.backend_rpc(|backend| {
            let fill = backend
                .partial
                .as_mut()
                .ok_or(Error::InvalidBackendResult)?;
            let read = backend
                .volume
                .read_at(&backend.path, base, &mut fill[..existing])
                .map_err(map)?;
            if read != existing {
                return Err(Error::InvalidBackendResult);
            }
            Ok(())
        })?;
        let first = input.slice(0, count)?;
        let Some(state) = &mut self.state else {
            return Err(Error::InvalidBackendResult);
        };
        let fill = self
            .backend
            .partial
            .as_ref()
            .ok_or(Error::InvalidBackendResult)?;
        state.cache_write(
            node,
            identity,
            length,
            offset,
            &first,
            Some(&fill[..existing]),
        )
    }
    pub(super) fn flush_writeback(&mut self) -> Result<(), Error> {
        self.flush_selected(None)
    }
    pub(super) fn flush_node(&mut self, node: u64) -> Result<(), Error> {
        self.flush_selected(Some(node))
    }
    fn flush_selected(&mut self, selected: Option<u64>) -> Result<(), Error> {
        self.check_writeback()?;
        self.writeback.freeze(selected);
        self.pump_batches(selected, usize::MAX)?;
        Ok(())
    }
    pub(super) fn flush_batch(&mut self, selected: Option<u64>) -> Result<bool, Error> {
        self.check_writeback()?;
        if self.writeback.frozen.is_empty() {
            self.writeback.freeze(selected);
        }
        self.pump_batches(selected, PRESSURE_BATCHES)
    }
    fn pump_batches(&mut self, selected: Option<u64>, budget: usize) -> Result<bool, Error> {
        let mut submitted = 0;
        loop {
            while submitted < budget {
                let Some(slot) = self.backend.batches.order.vacant() else {
                    break;
                };
                match self.submit_batch(selected, slot) {
                    Ok(false) => break,
                    Ok(true) => submitted += 1,
                    Err(error) => {
                        // Reap every acknowledged receipt before disabling transport.
                        return self
                            .finish_batches_with_failure(Some(error))
                            .map(|()| false);
                    }
                }
            }
            let Some(slot) = self.backend.batches.order.oldest() else {
                break;
            };
            // SetTimes is an ordinary ordered provider operation. Preserve the
            // existing full-window fence at a file/cut tail, including failure
            // ownership, rather than publishing metadata over pending data.
            let metadata_fence = self.backend.batches.order.active().any(|index| {
                let batch = &self.backend.batches.slots[index];
                batch.last && batch.modified.is_some()
            });
            if metadata_fence || submitted == budget {
                self.finish_batches()?;
            } else {
                if let Err(error) = self.complete_batch(slot) {
                    return self
                        .finish_batches_with_failure(Some(error))
                        .map(|()| false);
                }
                if let Err(error) = self.retire_batch(slot) {
                    return self
                        .finish_batches_with_failure(Some(error))
                        .map(|()| false);
                }
            }
            if submitted == budget {
                break;
            }
        }
        Ok(submitted != 0)
    }
    fn submit_batch(&mut self, selected: Option<u64>, slot: usize) -> Result<bool, Error> {
        let Some(batch) = queue::select(
            &self.writeback.frozen,
            selected,
            self.writeback.cursor,
            BATCH_BYTES,
            |page| page.value().bytes().len(),
        ) else {
            return Ok(false);
        };
        let Some(state) = &mut self.state else {
            return Err(Error::InvalidBackendResult);
        };
        let path = state.records.path(batch.node)?;
        let backend = &mut self.backend;
        let target = &mut backend.batches.slots[slot];
        if !target.pages.is_empty() {
            return Err(Error::InvalidBackendResult);
        }
        target.path.clear();
        target.path.push_str(path);
        target.node = batch.node;
        target.offset = batch.offset;
        target.length = batch.length;
        target.modified = batch.modified;
        target.last = batch.last;
        target.progress = BatchProgress::Prepared;
        queue::take(&mut state.writeback.frozen, &batch, &mut target.pages);
        state.writeback.cursor = batch.node;
        state.writeback.flights[slot] = Some(Flight {
            node: batch.node,
            end: batch.offset + batch.length as u64,
            modified: batch.modified,
            pages: target.pages.len(),
            bytes: batch.length,
        });
        if !self.backend.batches.order.admit(slot) {
            return Err(Error::InvalidBackendResult);
        }
        let result = self.backend_rpc(|backend| {
            let batch = &mut backend.batches.slots[slot];
            let input = WriteSource::fragments(batch)?;
            if backend.volume.supports_async_writeback().map_err(map)? {
                let (token, accepted) = backend
                    .volume
                    .submit_writeback(&batch.path, batch.offset, &input)
                    .map_err(map)?;
                Ok(BatchProgress::Submitted { token, accepted })
            } else {
                let accepted = backend
                    .volume
                    .write_at(&batch.path, batch.offset, &input)
                    .map_err(map)?;
                Ok(BatchProgress::Completed { accepted })
            }
        });
        self.backend.batches.slots[slot].progress = match result {
            Ok(progress) => progress,
            Err(error) => {
                self.backend.batches.slots[slot].progress = BatchProgress::Failed(error);
                return Err(error);
            }
        };
        // A short acknowledged prefix still owns a receipt and must be reaped.
        // finish_batches reports NoSpace only after that completion succeeds.
        Ok(true)
    }
    pub(super) fn finish_batches(&mut self) -> Result<(), Error> {
        self.finish_batches_with_failure(None)
    }
    fn complete_batch(&mut self, slot: usize) -> Result<(), Error> {
        match self.backend.batches.slots[slot].progress {
            BatchProgress::Submitted { token, accepted } => {
                let result = self.backend_rpc(|backend| {
                    let completion = backend.volume.reap_writeback(token).map_err(map)?;
                    if completion.batch != token || completion.accepted != accepted as u64 {
                        backend.volume.health.fail();
                        return Err(Error::InvalidBackendResult);
                    }
                    completion.result.map_err(map)
                });
                self.backend.batches.slots[slot].progress = match result {
                    Ok(()) => BatchProgress::Completed { accepted },
                    Err(error) => BatchProgress::Failed(error),
                };
                result?;
            }
            BatchProgress::Failed(error) => {
                return Err(error);
            }
            BatchProgress::Prepared => {
                return Err(Error::InvalidBackendResult);
            }
            BatchProgress::Completed { .. } => {}
        }
        Ok(())
    }
    fn finish_batches_with_failure(&mut self, failure: Option<Error>) -> Result<(), Error> {
        let mut failure = self.writeback.failure.or(failure);
        for slot in self.backend.batches.order.active() {
            if let Err(error) = self.complete_batch(slot) {
                failure.get_or_insert(error);
            }
        }
        if let Some(error) = failure {
            return Err(self.fail_writeback(error));
        }
        while let Some(slot) = self.backend.batches.order.oldest() {
            self.retire_batch(slot).inspect_err(|error| {
                self.fail_writeback(*error);
            })?;
        }
        Ok(())
    }
    fn retire_batch(&mut self, slot: usize) -> Result<(), Error> {
        self.backend_rpc(|backend| {
            let batch = &backend.batches.slots[slot];
            let BatchProgress::Completed { accepted } = batch.progress else {
                return Err(Error::InvalidBackendResult);
            };
            if accepted != batch.length {
                // VFS already accepted the full dirty version. A provider's
                // short admission cannot leave an undetachable sticky mount.
                return Err(Error::Remote(BackendError::NoSpace));
            }
            if batch.last
                && let Some(modified) = batch.modified
            {
                backend
                    .volume
                    .set_times(&batch.path, None, Some(modified))
                    .map_err(map)?;
            }
            Ok(())
        })?;
        let summary = self.writeback.flights[slot].ok_or(Error::InvalidBackendResult)?;
        let modified = if self.backend.batches.slots[slot].last {
            summary.modified
        } else {
            None
        };
        let Some(state) = &mut self.state else {
            return Err(Error::InvalidBackendResult);
        };
        let path = &self.backend.batches.slots[slot].path;
        state.complete_attributes(summary.node, path, summary.end, modified)?;
        self.writeback.flights[slot] = None;
        self.backend.batches.slots[slot].pages.clear();
        if !self.backend.batches.order.retire(slot) {
            return Err(Error::InvalidBackendResult);
        }
        Ok(())
    }
    pub(super) fn sync_file_writeback(
        &mut self,
        node: u64,
        mode: hyper_filesystem::protocol::SyncMode,
    ) -> Result<(), Error> {
        self.flush_selected(Some(node))?;
        self.bind_path(node)?;
        self.rpc_bound(|volume, path| volume.sync_file(path, mode))
            .inspect_err(|error| {
                self.fail_writeback(*error);
            })
    }
    pub(super) fn sync_writeback(&mut self) -> Result<(), Error> {
        self.flush_writeback()?;
        self.rpc(|volume| volume.sync()).inspect_err(|error| {
            self.fail_writeback(*error);
        })?;
        self.writeback.changed = self.writeback.count() != 0;
        Ok(())
    }
}
