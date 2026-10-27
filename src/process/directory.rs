// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Weak discovery index for published native Processes.

use alloc::boxed::Box;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use hyper::mm::{WeakFallibleArc, try_box};
use hyper::sync::InterruptSpinLock;

use super::ProcessId;
use super::owner::{Process, ProcessError, ProcessInner, ProcessSnapshot};
use {crate::capability::HandleScanCursor, crate::capability::HandleSnapshotPage};

const PAGE_CAPACITY: usize = 8;
static NEXT_SEQUENCE: AtomicU64 = AtomicU64::new(1);

struct Entry {
    sequence: u64,
    id: ProcessId,
    process: WeakFallibleArc<ProcessInner>,
    next: Option<Box<Entry>>,
}

struct Directory {
    head: Option<Box<Entry>>,
}

type DirectoryLock<T> = InterruptSpinLock<T, crate::hal::irq::LocalMask>;

static DIRECTORY: DirectoryLock<Directory> = DirectoryLock::new(Directory { head: None });

pub(super) const fn registration_size() -> usize {
    core::mem::size_of::<Entry>()
}

/// Process-lifetime hook which removes diagnostic metadata before charges drop.
pub(super) struct Membership {
    id: ProcessId,
    published: AtomicBool,
}

impl Membership {
    pub(super) const fn new(id: ProcessId) -> Self {
        Self {
            id,
            published: AtomicBool::new(false),
        }
    }

    fn publish(&self) {
        if self.published.swap(true, Ordering::Release) {
            directory_invariant_violation();
        }
    }
}

impl Drop for Membership {
    fn drop(&mut self) {
        if self.published.load(Ordering::Acquire) {
            unregister(self.id);
        }
    }
}

/// Fully allocated directory node which remains invisible until publication.
#[must_use = "publish or discard the prepared Process registration"]
pub(super) struct PreparedRegistration {
    entry: Option<Box<Entry>>,
}

impl PreparedRegistration {
    pub(super) fn try_new(process: &Process) -> Result<Self, ProcessError> {
        let sequence = allocate_sequence()?;
        let entry = try_box(Entry {
            sequence,
            id: process.id(),
            process: process.inner.downgrade(),
            next: None,
        })
        .map_err(|_| ProcessError::Allocation)?;
        Ok(Self { entry: Some(entry) })
    }

    pub(super) fn publish(mut self, process: &Process) {
        let entry = match self.entry.take() {
            Some(entry) => entry,
            None => directory_invariant_violation(),
        };
        DIRECTORY.with(|directory| directory.insert(entry));
        process.inner.directory.publish();
    }
}

impl Drop for PreparedRegistration {
    fn drop(&mut self) {
        // Dropping an unpublished node is the ordinary abort path.
        drop(self.entry.take());
    }
}

fn allocate_sequence() -> Result<u64, ProcessError> {
    let mut current = NEXT_SEQUENCE.load(Ordering::Relaxed);
    loop {
        let next = current.checked_add(1).ok_or(ProcessError::Allocation)?;
        match NEXT_SEQUENCE.compare_exchange_weak(
            current,
            next,
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => return Ok(current),
            Err(observed) => current = observed,
        }
    }
}

fn unregister(id: ProcessId) {
    let removed = DIRECTORY.with(|directory| directory.remove(id));
    match removed {
        Some(entry) => drop(entry),
        None => directory_invariant_violation(),
    }
}

impl hyper::collections::linked_list::NextLink for Box<Entry> {
    fn next(&self) -> &Option<Self> {
        &self.next
    }
    fn next_mut(&mut self) -> &mut Option<Self> {
        &mut self.next
    }
}
impl Directory {
    fn insert(&mut self, entry: Box<Entry>) {
        if hyper::collections::linked_list::insert_before(&mut self.head, entry, |new, current| {
            current.sequence < new.sequence
        })
        .is_err()
        {
            directory_invariant_violation();
        }
    }
    fn remove(&mut self, id: ProcessId) -> Option<Box<Entry>> {
        hyper::collections::linked_list::remove_first(&mut self.head, |entry| entry.id == id)
    }
}

/// Position in a newest-first scan of published Processes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ProcessScanCursor {
    before_sequence: u64,
}

impl ProcessScanCursor {
    pub(crate) const fn start() -> Self {
        Self {
            before_sequence: u64::MAX,
        }
    }

    pub(crate) const fn from_token(token: u64) -> Self {
        if token == 0 {
            Self::start()
        } else {
            Self {
                before_sequence: token,
            }
        }
    }

    pub(crate) const fn token(self) -> u64 {
        self.before_sequence
    }
}

/// One pinned Process entry used only while producing diagnostic snapshots.
pub(crate) struct ProcessDiagnosticRef {
    process: Process,
}

impl ProcessDiagnosticRef {
    pub(crate) fn inspect_handle_object(
        &self,
        value: crate::capability::HandleValue,
    ) -> Result<crate::object::ErasedKernelRef<crate::object::Diagnostic>, super::ProcessError>
    {
        self.process.inspect_handle_object(value)
    }
    pub(crate) fn clone_diagnostic(&self) -> Self {
        Self {
            process: self.process.clone(),
        }
    }

    /// Captures lifecycle metadata without exposing the operation-capable
    /// Process owner retained by this diagnostic pin.
    pub(crate) fn snapshot(&self) -> ProcessSnapshot {
        self.process.snapshot()
    }

    /// Captures one pointer-free page of this Process's handle edges.
    pub(crate) fn scan_handles(
        &self,
        cursor: HandleScanCursor,
    ) -> Result<HandleSnapshotPage, ProcessError> {
        self.process.scan_handles(cursor)
    }
}

pub(crate) struct ProcessSnapshotPage {
    entries: [Option<ProcessDiagnosticRef>; PAGE_CAPACITY],
    len: usize,
    next: Option<ProcessScanCursor>,
}

impl ProcessSnapshotPage {
    pub(crate) fn entries(&self) -> impl Iterator<Item = &ProcessDiagnosticRef> {
        self.entries[..self.len].iter().filter_map(Option::as_ref)
    }

    pub(crate) const fn next(&self) -> Option<ProcessScanCursor> {
        self.next
    }
}

pub(crate) fn scan(cursor: ProcessScanCursor) -> ProcessSnapshotPage {
    let mut entries: [Option<ProcessDiagnosticRef>; PAGE_CAPACITY] =
        [const { None }; PAGE_CAPACITY];
    let (len, next) = DIRECTORY.with(|directory| {
        let mut len = 0;
        let mut last_sequence = 0;
        let mut entry = directory.head.as_deref();
        while let Some(current) = entry {
            if current.sequence < cursor.before_sequence
                && let Some(inner) = current.process.upgrade()
            {
                entries[len] = Some(ProcessDiagnosticRef {
                    process: Process { inner },
                });
                len += 1;
                last_sequence = current.sequence;
                if len == PAGE_CAPACITY {
                    break;
                }
            }
            entry = current.next.as_deref();
        }
        let more = len == PAGE_CAPACITY && has_older_live(directory.head.as_deref(), last_sequence);
        let next = more.then_some(ProcessScanCursor {
            before_sequence: last_sequence,
        });
        (len, next)
    });
    ProcessSnapshotPage { entries, len, next }
}

/// One finite newest-first process scan and increasing-address mapping scan.
///
/// At most one Process is pinned between batches. Advancing past a range before
/// reclaim prevents refaulting hot pages, commit conflicts and dirty pages from
/// keeping a request alive indefinitely. Newer Processes are outside the scan.
pub(crate) struct FilePageReclaimCursor {
    processes: ProcessScanCursor,
    current: Option<MappingReclaimProcess>,
    finished: bool,
}

struct MappingReclaimProcess {
    // Process retirement is lifecycle-driven, not delayed by this metadata
    // pin. Never retain its address-space Arc across worker batches: doing so
    // would delay final address-space retirement while the worker sleeps.
    process: Process,
    next_address: u64,
}

impl FilePageReclaimCursor {
    pub(crate) const fn start() -> Self {
        Self {
            processes: ProcessScanCursor::start(),
            current: None,
            finished: false,
        }
    }
}

/// One bounded page-reclaim pass. Registry pins are acquired under the index
/// lock; mapping replacement and TLB retirement run after releasing it. The
/// limit bounds examined virtual pages, not just pages successfully reclaimed.
pub(crate) fn reclaim_file_pages(
    cursor: &mut FilePageReclaimCursor,
    limit: usize,
    domain: Option<crate::accounting::ResourceDomainId>,
) -> (usize, bool) {
    if cursor.finished || limit == 0 {
        return (0, cursor.finished);
    }
    let mut examined = 0;
    let mut reclaimed = 0;
    for _ in 0..PAGE_CAPACITY {
        if cursor.current.is_none() {
            cursor.current =
                next_reclaim_process(&mut cursor.processes).map(|process| MappingReclaimProcess {
                    process,
                    next_address: 0,
                });
            if cursor.current.is_none() {
                cursor.finished = true;
                break;
            }
        }
        let current = match cursor.current.as_mut() {
            Some(current) => current,
            None => break,
        };
        if domain.is_some_and(|id| !current.process.resource_domain().charges_domain(id)) {
            cursor.current = None;
            continue;
        }
        let Ok(space) = current.process.address_space_owner() else {
            cursor.current = None;
            continue;
        };
        let Some(range) = space.next_file_reclaim_range(current.next_address, limit - examined)
        else {
            cursor.current = None;
            continue;
        };
        current.next_address = range.end().get();
        examined += (range.length() / hyper::mm::PAGE_SIZE) as usize;
        // A conflicting map/unmap or pinned write skips this range in the
        // finite pass; retrying it here could starve every remaining process.
        reclaimed += space.reclaim_file_range(range).unwrap_or(0);
        if examined >= limit {
            break;
        }
    }
    (reclaimed, cursor.finished)
}

fn next_reclaim_process(cursor: &mut ProcessScanCursor) -> Option<Process> {
    DIRECTORY.with(|directory| {
        let mut entry = directory.head.as_deref();
        while let Some(current) = entry {
            if current.sequence < cursor.before_sequence {
                cursor.before_sequence = current.sequence;
                if let Some(inner) = current.process.upgrade() {
                    return Some(Process { inner });
                }
            }
            entry = current.next.as_deref();
        }
        None
    })
}

fn has_older_live(mut entry: Option<&Entry>, sequence: u64) -> bool {
    while let Some(current) = entry {
        if current.sequence < sequence && current.process.is_alive() {
            return true;
        }
        entry = current.next.as_deref();
    }
    false
}

#[cold]
fn directory_invariant_violation() -> ! {
    hyper::debug::invariant_failure("process directory invariant")
}
