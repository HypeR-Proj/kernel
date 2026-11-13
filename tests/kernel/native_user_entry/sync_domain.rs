// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Real domain, Thread-detach and multi-source wait lifecycle checks.

mod admission;

use super::{Error, prepare_process, retire_process};
use crate::accounting::{ResourceDomain, ResourceKind, ResourceLimits};
use crate::authority::Rights;
use crate::capability::HandleValue;
use crate::mm::user_space::{NativeSharedAtomicWord, VmoObject};
use crate::object::{Event, KernelObject, KernelRef, OperationPin, PreparedCancellation};
use crate::process::sync_domain::{OwnerState, SyncDomain};
use crate::process::sync_wait::{self, Outcome, Word};
use crate::process::{MachineAbi, Process, TaskGroup, TerminalReason, UserThread};
use crate::task::{TEST_PROGRESS_TIMEOUT_NS, scheduler, wait_for_test_progress};
use alloc::boxed::Box;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use hyper::mm::{FallibleArc, PAGE_SIZE, try_box};

type Domain = KernelRef<SyncDomain, OperationPin>;
type Source = KernelRef<VmoObject, OperationPin>;
const WAITERS: u64 = hyper::abi::native::HYPER_NATIVE_SYNC_MUTEX_WAITERS;

pub(super) fn run(parent: &ResourceDomain, group: &TaskGroup, program: &[u8]) -> Result<(), Error> {
    let sponsor = parent
        .try_new_child(ResourceLimits::UNLIMITED)
        .map_err(|_| Error::Construction)?;
    let process = prepare_process(&sponsor, group, program, MachineAbi::Aarch64)?;
    let result = exercise(&process);
    process.request_stop(TerminalReason::Requested);
    retire_process(&process)?;
    result?;
    crate::pr_info!(
        "HypeR test: SyncDomain real Thread death, stale-owner handoff, quotas and retirement passed"
    );
    Ok(())
}

fn make_domain(
    process: &Process,
    sponsor: &ResourceDomain,
) -> Result<(HandleValue, Domain), Error> {
    let handle = process
        .create_object(
            SyncDomain::try_new(sponsor).map_err(|_| Error::Construction)?,
            SyncDomain::SUPPORTED_RIGHTS,
        )
        .map_err(|_| Error::Construction)?;
    let object = process
        .resolve_handle::<SyncDomain>(handle, Rights::INSPECT)
        .map_err(|_| Error::Construction)?
        .into_operation_pin();
    Ok((handle, object))
}

fn exercise(process: &Process) -> Result<(), Error> {
    let sponsor =
        ResourceDomain::try_new_root(ResourceLimits::UNLIMITED).map_err(|_| Error::Construction)?;
    let baseline = sponsor.usage();
    let denied = sponsor
        .try_new_child(ResourceLimits::UNLIMITED.with(ResourceKind::KernelMemoryBytes, 0))
        .map_err(|_| Error::Construction)?;
    let (handle, domain) = make_domain(process, &sponsor)?;
    let b = process
        .create_initial_user_thread("sync-domain/owner-b", scheduler::CpuMask::ALL)
        .map_err(|_| Error::Construction)?;
    let d = process
        .create_initial_user_thread("sync-domain/owner-d", scheduler::CpuMask::ALL)
        .map_err(|_| Error::Construction)?;
    // Keep the Process alive while the two observed owners are truly detached.
    let survivor = process
        .create_initial_user_thread("sync-domain/survivor", scheduler::CpuMask::ALL)
        .map_err(|_| Error::Construction)?;
    if SyncDomain::join(&domain, &b, &denied).is_ok()
        || domain.object().owner_query(1) != OwnerState::Invalid
    {
        return Err(Error::AtomicWait("failed JOIN published an identity"));
    }
    let b_token = SyncDomain::join(&domain, &b, &sponsor).map_err(|_| Error::Construction)?;
    let d_token = SyncDomain::join(&domain, &d, &sponsor).map_err(|_| Error::Construction)?;
    if b_token != 1 || d_token != 2 || SyncDomain::join(&domain, &b, &denied).ok() != Some(b_token)
    {
        return Err(Error::AtomicWait("JOIN identity/idempotence"));
    }
    let vmo_handle = process
        .create_object(
            VmoObject::try_new_writable(PAGE_SIZE, &sponsor).map_err(|_| Error::Construction)?,
            Rights::READ
                .union(Rights::WRITE)
                .union(Rights::SYNC_CONFIGURE),
        )
        .map_err(|_| Error::Construction)?;
    let source = process
        .resolve_handle::<VmoObject>(vmo_handle, Rights::WRITE)
        .map_err(|_| Error::Construction)?
        .into_operation_pin();
    if SyncDomain::bind(&domain, source.object(), 0, PAGE_SIZE, &denied).is_ok()
        || source.object().sync_key(domain.koid().get(), 0).is_ok()
    {
        return Err(Error::AtomicWait("failed bind published attribution"));
    }
    SyncDomain::bind(&domain, source.object(), 0, PAGE_SIZE, &sponsor)
        .map_err(|_| Error::Construction)?;
    source
        .object()
        .write(0, &0_u64.to_le_bytes())
        .map_err(|_| Error::Construction)?;
    let pin = source
        .object()
        .pin_shared_atomic_u64(0)
        .map_err(|_| Error::Construction)?;
    let cancel_handle = process
        .create_object(
            Event::try_new(&sponsor).map_err(|_| Error::Construction)?,
            Event::SUPPORTED_RIGHTS,
        )
        .map_err(|_| Error::Construction)?;
    let cancellation = process
        .resolve_handle::<Event>(cancel_handle, Rights::WAIT)
        .map_err(|_| Error::Construction)?
        .into_operation_pin();
    let state = FallibleArc::try_new(State {
        process: process.clone(),
        domain: domain.clone(),
        source: source.clone(),
        vmo_handle,
        pin,
        expected: b_token | WAITERS,
        observed: b_token,
        cancellation,
        armed: AtomicUsize::new(0),
        finished: AtomicUsize::new(0),
        cancelled: AtomicUsize::new(0),
        result: AtomicUsize::new(0),
    })
    .map_err(|_| Error::Construction)?;
    store(&state, b_token | WAITERS)?;
    immediate_cases(&state, &denied)?;
    admission::run(&state)?;
    // Child-domain metadata is charged to its parent independently of the
    // objects exercised below. Release that fixture before checking baseline.
    drop(denied);
    // Twenty survivors exceed the sixteen-ticket dispatch batch. Cancel one
    // additional registration after publication, before the owner exit.
    run_batch(&state, 21, true, || {
        state
            .cancellation
            .object()
            .signal(0, Event::SIGNALED.bits())
            .map_err(|_| Error::Construction)?;
        if !wait_for_test_progress(TEST_PROGRESS_TIMEOUT_NS, || {
            Ok::<_, Error>(state.cancelled.load(Ordering::Acquire) == 1)
        })? {
            return Err(Error::AtomicWait("queued Event cancellation"));
        }
        store(&state, d_token | WAITERS)?;
        stop(&d)
    })?;
    if domain.object().owner_query(b_token) != OwnerState::Live
        || domain.object().owner_query(d_token) != OwnerState::Dead
    {
        return Err(Error::AtomicWait("new-owner death classification"));
    }
    store(&state, d_token | WAITERS)?;
    let word = Word::prepare(process, source.clone(), domain.koid().get(), 0)
        .map_err(|_| Error::Construction)?;
    if sync_wait::wait(
        process,
        domain.clone(),
        word,
        request(&state, d_token | WAITERS, d_token, u64::MAX),
        None,
        || false,
    )
    .map_err(|_| Error::AtomicWait("dead owner admission"))?
        != Outcome::OwnerExited(d_token)
    {
        return Err(Error::AtomicWait("dead owner not distinguished"));
    }
    // A second exit after the first finite cut drained must rearm work.
    store(&state, b_token | WAITERS)?;
    run_batch(&state, 3, false, || stop(&b))?;
    drop(state);
    process
        .close_handle(cancel_handle)
        .map_err(|_| Error::Lifecycle)?;
    process
        .close_handle(vmo_handle)
        .map_err(|_| Error::Lifecycle)?;
    drop(source);
    process.close_handle(handle).map_err(|_| Error::Lifecycle)?;
    drop(domain);
    if !wait_for_test_progress(TEST_PROGRESS_TIMEOUT_NS, || {
        Ok::<_, Error>(sponsor.usage() == baseline)
    })? {
        return Err(Error::AtomicWait("domain and VMO final retirement"));
    }
    domain_retirement(process, &survivor, &sponsor)?;
    Ok(())
}

fn immediate_cases(state: &State, denied: &ResourceDomain) -> Result<(), Error> {
    if state.domain.object().prepare_wait(denied).is_ok() {
        return Err(Error::AtomicWait("domain wait quota accepted"));
    }
    let account = state.process.resource_domain();
    let baseline = account.usage();
    for (expected, owner, deadline, cancel, wanted) in [
        (0, state.observed, u64::MAX, false, Outcome::Recheck),
        (state.expected, state.observed, 0, false, Outcome::TimedOut),
        (
            state.expected,
            state.observed,
            u64::MAX,
            true,
            Outcome::Cancelled,
        ),
    ] {
        let word = Word::prepare(
            &state.process,
            state.source.clone(),
            state.domain.koid().get(),
            0,
        )
        .map_err(|_| Error::Construction)?;
        let got = sync_wait::wait(
            &state.process,
            state.domain.clone(),
            word,
            request(state, expected, owner, deadline),
            None,
            || cancel,
        )
        .map_err(|_| Error::AtomicWait("immediate domain wait"))?;
        if got != wanted || account.usage() != baseline {
            return Err(Error::AtomicWait(
                "domain immediate outcome/resource cleanup",
            ));
        }
    }
    let word = Word::prepare(
        &state.process,
        state.source.clone(),
        state.domain.koid().get(),
        0,
    )
    .map_err(|_| Error::Construction)?;
    if sync_wait::wait(
        &state.process,
        state.domain.clone(),
        word,
        request(state, state.expected, 99, u64::MAX),
        None,
        || false,
    )
    .is_ok()
        || account.usage() != baseline
    {
        return Err(Error::AtomicWait("unknown owner or admission cleanup"));
    }
    for resource in [ResourceKind::KernelMemoryBytes, ResourceKind::Timers] {
        let word = Word::prepare(
            &state.process,
            state.source.clone(),
            state.domain.koid().get(),
            0,
        )
        .map_err(|_| Error::Construction)?;
        account
            .set_local_limits(
                ResourceLimits::UNLIMITED.with(resource, account.usage().total(resource)),
            )
            .map_err(|_| Error::Construction)?;
        let deadline =
            crate::time::monotonic_nanoseconds().map_err(|_| Error::Scheduler)? + 1_000_000_000;
        let result = sync_wait::wait(
            &state.process,
            state.domain.clone(),
            word,
            request(state, state.expected, state.observed, deadline),
            None,
            || false,
        );
        account
            .set_local_limits(ResourceLimits::UNLIMITED)
            .map_err(|_| Error::Construction)?;
        if result.is_ok() || account.usage() != baseline {
            return Err(Error::AtomicWait("failed domain wait did not roll back"));
        }
    }
    Ok(())
}

fn domain_retirement(
    process: &Process,
    survivor: &UserThread,
    sponsor: &ResourceDomain,
) -> Result<(), Error> {
    let baseline = sponsor.usage();
    for _ in 0..16 {
        let (handle, domain) = make_domain(process, sponsor)?;
        let token =
            SyncDomain::join(&domain, survivor, sponsor).map_err(|_| Error::Construction)?;
        if token != 1 {
            return Err(Error::AtomicWait("fresh domain identity"));
        }
        process.close_handle(handle).map_err(|_| Error::Lifecycle)?;
        drop(domain);
        if !wait_for_test_progress(TEST_PROGRESS_TIMEOUT_NS, || {
            Ok::<_, Error>(sponsor.usage() == baseline)
        })? {
            return Err(Error::AtomicWait(
                "unused domain retained survivor membership",
            ));
        }
    }
    Ok(())
}

fn stop(thread: &UserThread) -> Result<(), Error> {
    scheduler::request_user_thread_stop(
        thread.scheduler_id().ok_or(Error::Construction)?,
        TerminalReason::Requested,
    )
    .map_err(|_| Error::Scheduler)?;
    if !wait_for_test_progress(TEST_PROGRESS_TIMEOUT_NS, || {
        Ok::<_, Error>(thread.try_join().is_some())
    })? {
        return Err(Error::AtomicWait("owner actual detach"));
    }
    Ok(())
}

struct State {
    process: Process,
    domain: Domain,
    source: Source,
    vmo_handle: HandleValue,
    pin: NativeSharedAtomicWord,
    expected: u64,
    observed: u64,
    cancellation: KernelRef<Event, OperationPin>,
    armed: AtomicUsize,
    finished: AtomicUsize,
    cancelled: AtomicUsize,
    result: AtomicUsize,
}
fn store(state: &State, value: u64) -> Result<(), Error> {
    let address =
        crate::mm::memory::linear_address(state.pin.physical()).ok_or(Error::Construction)?;
    // SAFETY: this fixture retains its aligned 64-bit RAM word in `pin`; the
    // only concurrent accesses are the production atomic-load seam. No native
    // process receives a mapping or can issue mixed-width writes to this page.
    unsafe { &*core::ptr::with_exposed_provenance::<AtomicU64>(address) }
        .store(value, Ordering::Release);
    Ok(())
}
fn request(
    state: &State,
    expected: u64,
    owner: u64,
    deadline: u64,
) -> crate::abi::native::SyncWaitRequest {
    crate::abi::native::SyncWaitRequest {
        vmo: state.vmo_handle,
        offset: 0,
        expected,
        owner,
        deadline,
        cancellation: None,
    }
}
fn run_batch(
    state: &FallibleArc<State>,
    count: usize,
    cancel_one: bool,
    action: impl FnOnce() -> Result<(), Error>,
) -> Result<(), Error> {
    state.armed.store(0, Ordering::Release);
    state.finished.store(0, Ordering::Release);
    state.cancelled.store(0, Ordering::Release);
    state.result.store(0, Ordering::Release);
    let mut workers = [None; 21];
    let result = (|| {
        for (index, slot) in workers.iter_mut().take(count).enumerate() {
            *slot = Some(spawn_waiter(state, cancel_one && index == 0)?);
        }
        if !wait_for_test_progress(TEST_PROGRESS_TIMEOUT_NS, || {
            Ok::<_, Error>(
                state.armed.load(Ordering::Acquire) == count
                    || state.finished.load(Ordering::Acquire) != 0,
            )
        })? || state.finished.load(Ordering::Acquire) != 0
        {
            return Err(Error::AtomicWait("domain batch admission"));
        }
        action()?;
        if !wait_for_test_progress(TEST_PROGRESS_TIMEOUT_NS, || {
            Ok::<_, Error>(state.finished.load(Ordering::Acquire) == count)
        })? || state.result.load(Ordering::Acquire) != 0
            || state.cancelled.load(Ordering::Acquire) != usize::from(cancel_one)
        {
            return Err(Error::AtomicWait("domain exit batch outcome"));
        }
        Ok(())
    })();
    // Do not use global test quiescence while dormant Native owners survive.
    // Join exactly the transient kernel workers created by this batch.
    let (backing, offset) = state
        .source
        .object()
        .sync_key(state.domain.koid().get(), 0)
        .map_err(|_| Error::Construction)?;
    // Changing expected closes the setup-error race with workers not yet parked.
    store(state, 0)?;
    let _ = sync_wait::wake(
        sync_wait::Key::new(state.domain.koid().get(), backing, offset),
        u32::MAX,
    );
    for worker in workers.into_iter().flatten() {
        if !wait_for_test_progress(TEST_PROGRESS_TIMEOUT_NS, || {
            Ok::<_, Error>(matches!(
                scheduler::thread_name(worker),
                Err(scheduler::Error::ThreadNotFound)
            ))
        })? {
            return Err(Error::AtomicWait("domain worker final retirement"));
        }
    }
    result
}

struct WorkerInput {
    state: FallibleArc<State>,
    cancellation: bool,
}
fn spawn_waiter(
    state: &FallibleArc<State>,
    cancellation: bool,
) -> Result<crate::task::thread::ThreadId, Error> {
    let pointer = Box::into_raw(
        try_box(WorkerInput {
            state: state.clone(),
            cancellation,
        })
        .map_err(|_| Error::Construction)?,
    );
    let thread = match scheduler::kthread_create(
        "sync-domain/stale-owner",
        waiter,
        pointer.expose_provenance(),
    ) {
        Ok(thread) => thread,
        Err(_) => {
            // SAFETY: failure did not publish a worker; this remains our Box.
            drop(unsafe { Box::from_raw(pointer) });
            return Err(Error::Scheduler);
        }
    };
    if !matches!(scheduler::thread_ready(thread), Ok(true)) {
        scheduler::discard_dormant_kernel_thread(thread).map_err(|_| Error::Scheduler)?;
        // SAFETY: the dormant worker was discarded without invoking its entry.
        drop(unsafe { Box::from_raw(pointer) });
        return Err(Error::Scheduler);
    }
    Ok(thread)
}
extern "C" fn waiter(argument: usize) {
    // SAFETY: spawn_waiter transfers one Box to this exact worker invocation.
    let input = unsafe {
        Box::from_raw(core::ptr::with_exposed_provenance_mut::<WorkerInput>(
            argument,
        ))
    };
    let state = &input.state;
    let cancellation = if input.cancellation {
        PreparedCancellation::new(state.cancellation.clone(), &state.process.resource_domain())
            .map(Some)
    } else {
        Ok(None)
    };
    let result = cancellation
        .map_err(sync_wait::Error::from)
        .and_then(|cancellation| {
            Word::prepare(
                &state.process,
                state.source.clone(),
                state.domain.koid().get(),
                0,
            )
            .and_then(|word| {
                sync_wait::wait(
                    &state.process,
                    state.domain.clone(),
                    word,
                    request(state, state.expected, state.observed, u64::MAX),
                    cancellation,
                    || {
                        state.armed.fetch_add(1, Ordering::Release);
                        false
                    },
                )
            })
        });
    match result {
        Ok(Outcome::Recheck) if !input.cancellation => {}
        Ok(Outcome::Cancelled) if input.cancellation => {
            state.cancelled.fetch_add(1, Ordering::Release);
        }
        _ => {
            state.result.fetch_add(1, Ordering::Release);
        }
    }
    state.finished.fetch_add(1, Ordering::Release);
}
