// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Best-effort speculative reads on a dedicated normal-context worker.
//!
//! Queue entries retain weak filesystem/node/cache identities only. Closing
//! the last open can destroy its node even while work is running. Predictions
//! upgrade only the shared content record and use its mutation gate; they
//! never create a namespace lease that would keep a closed file busy. The
//! cache reclaimer never calls a backend or waits for this worker.

use alloc::vec::Vec;
use core::alloc::Layout;
use core::sync::atomic::{AtomicBool, Ordering};

use hyper::mm::{FallibleArc, WeakFallibleArc};
use hyper::sync::InterruptSpinLock;

use crate::accounting::CommittedCharge;
use crate::fs::cache::read_ahead::Range;
use crate::mm::cache_memory;
use crate::sync::Completion;
use crate::task::scheduler;
use {crate::fs::cache::FileDataCache, crate::fs::cache::FileIdentity};

use super::file_record::CachePage;
use super::instance::{FilesystemInstance, NodeLease, ReadAheadFile};

const QUEUE_CAPACITY: usize = 16;
static READY: AtomicBool = AtomicBool::new(false);
static WAKE: Completion = Completion::new();
static QUEUE: InterruptSpinLock<Queue, crate::hal::irq::LocalMask> =
    InterruptSpinLock::new(Queue {
        jobs: [const { None }; QUEUE_CAPACITY],
        head: 0,
        len: 0,
    });

struct Job {
    filesystem: WeakFallibleArc<FilesystemInstance>,
    file: ReadAheadFile,
    cache: WeakFallibleArc<FileDataCache<CachePage>>,
    identity: FileIdentity,
    range: Range,
    // Last field: charge outlives weak control-block retention on queue drop.
    _charge: CommittedCharge,
}

struct Queue {
    jobs: [Option<Job>; QUEUE_CAPACITY],
    head: usize,
    len: usize,
}

/// Called once alongside cache maintenance after scheduler/time setup.
pub(crate) fn initialize() -> Result<(), scheduler::Error> {
    let worker = scheduler::kthread_create("kvfs-readahead", worker_entry, 0)?;
    if READY
        .compare_exchange(false, true, Ordering::Release, Ordering::Relaxed)
        .is_err()
    {
        return Err(scheduler::Error::AlreadyInitialized);
    }
    if !scheduler::thread_ready(worker)? {
        return Err(scheduler::Error::InvalidThreadState);
    }
    Ok(())
}

/// Called only after a successful demand read has released its content gate.
/// Full queues, pressure, and already queued files discard the prediction.
pub(super) fn submit(
    filesystem: &FallibleArc<FilesystemInstance>,
    node: &NodeLease,
    cache: &FallibleArc<FileDataCache<CachePage>>,
    identity: FileIdentity,
    range: Range,
) {
    if !READY.load(Ordering::Acquire) || !cache.prefetch_allowed() {
        return;
    }
    let Some(node) = node.downgrade_cacheable() else {
        return;
    };
    let Ok(charge) = filesystem.reserve_read_ahead_metadata() else {
        return;
    };
    let mut job = Some(Job {
        filesystem: filesystem.downgrade(),
        file: node,
        cache: cache.downgrade(),
        identity,
        range,
        _charge: charge,
    });
    let queued = QUEUE.with(|queue| {
        if queue.len == QUEUE_CAPACITY
            || queue
                .jobs
                .iter()
                .flatten()
                .any(|old| old.identity == identity)
        {
            return false;
        }
        let tail = (queue.head + queue.len) % QUEUE_CAPACITY;
        queue.jobs[tail] = job.take();
        queue.len += 1;
        true
    });
    // Rejected weak references are destroyed outside the queue's IRQ lock.
    drop(job);
    if queued && WAKE.complete().is_err() {
        hyper::debug::invariant_failure("VFS read-ahead wake failed");
    }
}

#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
pub(crate) fn permanent_worker_count_for_test() -> usize {
    usize::from(READY.load(Ordering::Acquire))
}

extern "C" fn worker_entry(_: usize) {
    loop {
        if WAKE.wait().is_err() {
            hyper::debug::invariant_failure("VFS read-ahead wait failed");
        }
        let job = QUEUE.with(|queue| {
            if queue.len == 0 {
                return None;
            }
            let job = queue.jobs[queue.head].take();
            queue.head = (queue.head + 1) % QUEUE_CAPACITY;
            queue.len -= 1;
            job
        });
        if let Some(job) = job {
            run(job);
        }
        if scheduler::cond_resched().is_err() {
            hyper::debug::invariant_failure("VFS read-ahead reschedule failed");
        }
    }
}

fn run(job: Job) {
    let (Some(filesystem), Some(record), Some(cache)) = (
        job.filesystem.upgrade(),
        job.file.content_if_open(),
        job.cache.upgrade(),
    ) else {
        return;
    };
    if !cache.prefetch_allowed() {
        return;
    }
    // One worker owns at most one 512-KiB scratch allocation. Admit its full
    // allocator footprint against the same free-memory reserve as cache data.
    let Ok(layout) = Layout::array::<u8>(job.range.length) else {
        return;
    };
    let Some(pages) = cache_memory::allocation_page_bound(layout) else {
        return;
    };
    let Some(reservation) = cache_memory::reserve_metadata(pages) else {
        return;
    };
    let mut scratch = Vec::new();
    if scratch.try_reserve_exact(job.range.length).is_err() {
        return;
    }
    scratch.resize(job.range.length, 0);
    drop(reservation);
    // Failure belongs to an optional prediction. Demand rechecks the backend
    // health and revision, so neither failure nor stale work can forge a hit.
    let _ = filesystem.prefetch_file(
        &record,
        &cache,
        job.identity,
        job.range.offset,
        &mut scratch,
    );
}
