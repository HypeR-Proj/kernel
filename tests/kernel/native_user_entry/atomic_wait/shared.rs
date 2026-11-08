// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Shared-word admission and caller accounting through the real wait engine.

use super::*;
use crate::authority::Rights;
use crate::mm::user_space::{MemoryObjectError, VmoObject};
use crate::object::{KernelRef, OperationPin};

type Source = KernelRef<VmoObject, OperationPin>;
static SOURCE: InterruptSpinLock<Option<Source>, crate::hal::irq::LocalMask> =
    InterruptSpinLock::new(None);
static ARMED: AtomicBool = AtomicBool::new(false);
static FINISHED: AtomicBool = AtomicBool::new(false);
static CANCELLED: AtomicBool = AtomicBool::new(false);
static FAILED: AtomicBool = AtomicBool::new(false);

fn prepare(source: &Source, domain: &ResourceDomain) -> Result<atomic_wait::SharedWord, Error> {
    atomic_wait::SharedWord::prepare(source.clone(), 0, domain)
        .map_err(|_| Error::AtomicWait("shared word admission"))
}

pub(super) fn run(process: &Process) -> Result<(), Error> {
    // Backing belongs to an independent sponsor. Deferred object retirement
    // cannot alter the caller's later wait-accounting baselines.
    let sponsor =
        ResourceDomain::try_new_root(ResourceLimits::UNLIMITED).map_err(|_| Error::Construction)?;
    let object =
        VmoObject::try_new_writable(PAGE_SIZE, &sponsor).map_err(|_| Error::Construction)?;
    let handle = process
        .create_object(object, Rights::READ.union(Rights::WRITE))
        .map_err(|_| Error::Construction)?;
    let source = process
        .resolve_handle::<VmoObject>(handle, Rights::READ.union(Rights::WRITE))
        .map_err(|_| Error::Construction)?
        .into_operation_pin();
    let result = exercise(process, &source);
    let close = process.close_handle(handle).map_err(|_| Error::Lifecycle);
    result.and(close)
}

fn exercise(process: &Process, source: &Source) -> Result<(), Error> {
    let domain = process.resource_domain();
    // The enclosing atomic-wait fixture creates this isolated domain unlimited.
    let limits = ResourceLimits::UNLIMITED;
    let before = domain.usage();
    domain
        .set_local_limits(limits.with(
            ResourceKind::PinnedPages,
            before.total(ResourceKind::PinnedPages),
        ))
        .map_err(|_| Error::Construction)?;
    let denied = atomic_wait::SharedWord::prepare(source.clone(), 0, &domain);
    let unchanged = domain.usage() == before;
    domain
        .set_local_limits(limits)
        .map_err(|_| Error::Construction)?;
    if !matches!(
        denied,
        Err(MemoryObjectError::Resource(
            crate::accounting::ResourceError::LimitExceeded {
                resource: ResourceKind::PinnedPages,
                ..
            }
        ))
    ) || !unchanged
    {
        return Err(Error::AtomicWait("shared pin quota rejection"));
    }
    // A rejected pin must not leave writable admission behind. Warm backing
    // before all remaining comparisons, while the source handle stays live.
    source
        .object()
        .write(0, &[0; 4])
        .map_err(|_| Error::AtomicWait("rejected pin cleanup"))?;
    let baseline = domain.usage();
    for (expected, deadline, cancelled, outcome) in [
        (1, u64::MAX, false, WaitOutcome::Notified),
        (0, u64::MAX, true, WaitOutcome::Cancelled),
        (0, 0, false, WaitOutcome::TimedOut),
    ] {
        let word = prepare(source, &domain)?;
        if domain.usage().total(ResourceKind::PinnedPages)
            != baseline.total(ResourceKind::PinnedPages) + 1
        {
            return Err(Error::AtomicWait("shared caller pin charge"));
        }
        if atomic_wait::wait_shared(process, word, expected, deadline, || cancelled)
            .map_err(|_| Error::AtomicWait("shared immediate wait"))?
            != outcome
        {
            return Err(Error::AtomicWait("shared immediate outcome"));
        }
        verify_released(source, || domain.usage() == baseline)?;
    }
    for _ in 0..4 {
        let deadline = crate::time::monotonic_nanoseconds()
            .map_err(|_| Error::AtomicWait("shared clock"))?
            .checked_add(1_000_000)
            .ok_or(Error::AtomicWait("shared deadline overflow"))?;
        let word = prepare(source, &domain)?;
        if atomic_wait::wait_shared(process, word, 0, deadline, || false)
            .map_err(|_| Error::AtomicWait("shared timed wait"))?
            != WaitOutcome::TimedOut
        {
            return Err(Error::AtomicWait("shared timeout outcome"));
        }
        verify_released(source, || domain.usage() == baseline)?;
    }
    // Exercise failures after pin preparation, through production admission.
    // Restore limits before checking any outcome, including unexpected success.
    for resource in [ResourceKind::KernelMemoryBytes, ResourceKind::Timers] {
        let word = prepare(source, &domain)?;
        let used = domain.usage().total(resource);
        let deadline = crate::time::monotonic_nanoseconds()
            .map_err(|_| Error::AtomicWait("shared quota clock"))?
            .checked_add(1_000_000_000)
            .ok_or(Error::AtomicWait("shared quota deadline"))?;
        domain
            .set_local_limits(limits.with(resource, used))
            .map_err(|_| Error::Construction)?;
        let result = atomic_wait::wait_shared(process, word, 0, deadline, || false);
        domain
            .set_local_limits(limits)
            .map_err(|_| Error::Construction)?;
        if result.is_ok() {
            return Err(Error::AtomicWait("shared wait quota accepted"));
        }
        verify_released(source, || domain.usage() == baseline)?;
    }
    wake_registered(process, source)?;
    verify_released(source, || domain.usage() == baseline)?;
    crate::pr_info!("HypeR test: Shared atomic wait pin quotas and all retirement paths verified");
    Ok(())
}

fn verify_released(
    source: &Source,
    accounting_unchanged: impl FnOnce() -> bool,
) -> Result<(), Error> {
    if !accounting_unchanged() {
        return Err(Error::AtomicWait("shared waiter accounting release"));
    }
    source
        .object()
        .write(0, &[0; 4])
        .map_err(|_| Error::AtomicWait("shared word admission release"))?;
    if atomic_wait::wake_shared(source.koid(), 0, u32::MAX)
        .map_err(|_| Error::AtomicWait("shared cleanup wake"))?
        != 0
    {
        return Err(Error::AtomicWait("shared registration survived return"));
    }
    Ok(())
}

fn wake_registered(process: &Process, source: &Source) -> Result<(), Error> {
    SOURCE.with(|slot| *slot = Some(source.clone()));
    ARMED.store(false, Ordering::Release);
    FINISHED.store(false, Ordering::Release);
    CANCELLED.store(false, Ordering::Release);
    FAILED.store(false, Ordering::Release);
    let result = (|| {
        let id = scheduler::kthread_create("atomic-wait/shared", worker, 0)
            .map_err(|_| Error::Scheduler)?;
        if !matches!(scheduler::thread_ready(id), Ok(true)) {
            scheduler::discard_dormant_kernel_thread(id).map_err(|_| Error::Scheduler)?;
            return Err(Error::Scheduler);
        }
        if !wait_for_test_progress(TEST_PROGRESS_TIMEOUT_NS, || {
            Ok::<_, Error>(ARMED.load(Ordering::Acquire) || FINISHED.load(Ordering::Acquire))
        })? || FINISHED.load(Ordering::Acquire)
        {
            return Err(Error::AtomicWait("shared worker publication"));
        }
        // ARMED is set by the cancellation predicate while the bucket lock is
        // held. A wake acquiring that lock must observe the installed waiter.
        if atomic_wait::wake(process, ADDRESS, u32::MAX)
            .map_err(|_| Error::AtomicWait("private wake during shared wait"))?
            != 0
            || atomic_wait::wake_shared(source.koid(), 4, u32::MAX)
                .map_err(|_| Error::AtomicWait("other shared offset wake"))?
                != 0
            || atomic_wait::wake_shared(source.koid(), 0, 0)
                .map_err(|_| Error::AtomicWait("shared zero wake"))?
                != 0
            || atomic_wait::wake_shared(source.koid(), 0, 1)
                .map_err(|_| Error::AtomicWait("shared wake one"))?
                != 1
        {
            return Err(Error::AtomicWait("shared wake namespace/count"));
        }
        if !wait_for_test_progress(TEST_PROGRESS_TIMEOUT_NS, || {
            Ok::<_, Error>(FINISHED.load(Ordering::Acquire))
        })? || FAILED.load(Ordering::Acquire)
        {
            return Err(Error::AtomicWait("shared worker completion"));
        }
        Ok(())
    })();
    CANCELLED.store(true, Ordering::Release);
    let cleanup = atomic_wait::wake_shared(source.koid(), 0, u32::MAX)
        .map(|_| ())
        .map_err(|_| Error::AtomicWait("shared worker cleanup wake"));
    // Retain static owners on failure to quiesce, just like the existing
    // private-wait fixture, rather than freeing a parked worker's input.
    crate::kernel_tests::support::quiesce_workers().map_err(|_| Error::Scheduler)?;
    drop(SOURCE.with(Option::take));
    result.and(cleanup)
}

extern "C" fn worker(_: usize) {
    let process = PROCESS.with(|slot| slot.clone());
    let source = SOURCE.with(|slot| slot.clone());
    let result = match (process, source) {
        (Some(process), Some(source)) => {
            prepare(&source, &process.resource_domain()).and_then(|word| {
                atomic_wait::wait_shared(&process, word, 0, u64::MAX, || {
                    ARMED.store(true, Ordering::Release);
                    CANCELLED.load(Ordering::Acquire)
                })
                .map_err(|_| Error::AtomicWait("shared worker wait"))
            })
        }
        _ => Err(Error::AtomicWait("shared worker missing owner")),
    };
    if !matches!(result, Ok(WaitOutcome::Notified)) {
        FAILED.store(true, Ordering::Release);
    }
    FINISHED.store(true, Ordering::Release);
}
