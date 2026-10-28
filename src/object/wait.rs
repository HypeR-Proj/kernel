// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Blocking transactions for object-signal observation.

use alloc::vec::Vec;

use hyper::sync::InterruptMaskGuard;
use {
    crate::accounting::CommittedCharge, crate::accounting::ResourceAmount,
    crate::accounting::ResourceDomain, crate::accounting::ResourceError,
    crate::accounting::ResourceKind,
};
use {
    crate::task::ArmedTimeout, crate::task::PreparedTimeout, crate::task::TimedWaitError,
    crate::task::WaitMobility, crate::task::WaitOutcome, crate::task::WaitQueue,
};
use {crate::task::scheduler, crate::task::scheduler::WaitRegistration};

use super::signals::{PreparedSignalWait, SignalSource, SignalWaitError, SignalWaitOutcome};

/// One borrowed signal source in an ordered multi-object wait request.
#[derive(Clone, Copy)]
pub(crate) struct SignalWaitRequest<'object> {
    source: SignalSource<'object>,
    requested: u64,
}

impl<'object> SignalWaitRequest<'object> {
    pub(crate) const fn new(source: SignalSource<'object>, requested: u64) -> Self {
        Self { source, requested }
    }
}

/// Terminal result of one multi-object wait transaction.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SignalWaitManyOutcome {
    Observed {
        index: usize,
        snapshot: super::signals::SignalSnapshot,
    },
    TimedOut,
    Cancelled,
}

#[derive(Clone, Copy)]
struct CanonicalWait<'object> {
    source: SignalSource<'object>,
    requested: super::signals::SignalMask,
}

#[derive(Clone, Copy)]
struct ValidatedWait<'object> {
    source: SignalSource<'object>,
    requested: super::signals::SignalMask,
}

/// Failure before a signal, timeout, or cancellation outcome is selected.
#[derive(Debug)]
pub(crate) enum ObjectWaitError {
    AllocationSize,
    Deadline(crate::time::Error),
    InvalidSignals,
    Resource(ResourceError),
    Signal(SignalWaitError),
    Timer(TimedWaitError),
}

impl From<ResourceError> for ObjectWaitError {
    fn from(error: ResourceError) -> Self {
        Self::Resource(error)
    }
}

impl From<SignalWaitError> for ObjectWaitError {
    fn from(error: SignalWaitError) -> Self {
        Self::Signal(error)
    }
}

impl From<crate::time::Error> for ObjectWaitError {
    fn from(error: crate::time::Error) -> Self {
        Self::Deadline(error)
    }
}

impl From<TimedWaitError> for ObjectWaitError {
    fn from(error: TimedWaitError) -> Self {
        Self::Timer(error)
    }
}

enum WaitDeadline {
    Elapsed,
    Infinite,
    At(u64),
}

impl WaitDeadline {
    fn from_absolute_nanoseconds(nanoseconds: u64) -> Result<Self, ObjectWaitError> {
        if nanoseconds == hyper::abi::native::HYPER_NATIVE_DEADLINE_INFINITE {
            return Ok(Self::Infinite);
        }
        let deadline = crate::time::deadline_from_monotonic_nanoseconds(nanoseconds)?;
        if hyper::hal::timer::deadline_reached(crate::time::monotonic_ticks(), deadline) {
            Ok(Self::Elapsed)
        } else {
            Ok(Self::At(deadline))
        }
    }
}

/// Timer resources allocated and charged before scheduler publication.
enum PreparedWaitTimer {
    Infinite,
    Finite {
        deadline: u64,
        timeout: PreparedTimeout,
        _charge: CommittedCharge,
    },
}

impl PreparedWaitTimer {
    fn try_finite(domain: &ResourceDomain, deadline: u64) -> Result<Self, ObjectWaitError> {
        let bytes = allocation_bytes(PreparedTimeout::allocation_size())?;
        let charge = domain
            .reserve(
                ResourceAmount::ZERO
                    .with(ResourceKind::KernelMemoryBytes, bytes)
                    .with(ResourceKind::Timers, 1),
            )?
            .commit();
        let timeout = PreparedTimeout::try_new()?;
        Ok(Self::Finite {
            deadline,
            timeout,
            _charge: charge,
        })
    }

    fn arm(self, registration: &WaitRegistration) -> Result<ArmedWaitTimer, TimedWaitError> {
        match self {
            Self::Infinite => Ok(ArmedWaitTimer::Infinite),
            Self::Finite {
                deadline,
                timeout,
                _charge,
            } => Ok(ArmedWaitTimer::Finite {
                timeout: timeout.arm(registration.ticket(), deadline)?,
                _charge,
            }),
        }
    }
}

/// Exact timer ownership retained until the selected wait outcome is known.
enum ArmedWaitTimer {
    Infinite,
    Finite {
        timeout: ArmedTimeout,
        _charge: CommittedCharge,
    },
}

/// Result of preparing every fallible resource for a timed scheduler wait.
pub(crate) enum TimedWaitPreparation {
    /// The absolute deadline elapsed before scheduler publication.
    Completed(WaitOutcome),
    /// Timer and exact current-Thread wait generation are fully armed.
    Armed(PreparedTimedWait),
}

/// Fully prepared but condition-unpublished scheduler wait.
///
/// A condition owner may allocate its own waiter storage first, construct this
/// token, and then publish both its condition node and the scheduler wait while
/// holding one IRQ-masking condition lock. No allocation or timer setup remains
/// after this token is returned.
#[must_use = "publish or abort the prepared timed wait"]
pub(crate) struct PreparedTimedWait {
    registration: Option<WaitRegistration>,
    timer: Option<ArmedWaitTimer>,
}

/// Scheduler wait committed under a condition lock but not yet parked.
#[must_use = "complete the committed timed wait with the retained IRQ mask"]
pub(crate) struct PublishedTimedWait {
    park: Option<scheduler::PrepareWait>,
    timer: Option<ArmedWaitTimer>,
}

impl PreparedTimedWait {
    pub(crate) fn ticket(&self) -> crate::task::WaitTicket {
        match self.registration.as_ref() {
            Some(registration) => registration.ticket(),
            None => object_wait_invariant("prepared wait lost registration", None),
        }
    }

    /// Resolves cancellation before condition publication. The exact winner is
    /// consumed later by `publish_locked`, so this never leaves an unowned wait.
    pub(crate) fn request_cancellation(&self) {
        if let Err(error) = scheduler::resolve_wait(self.ticket(), WaitOutcome::Cancelled) {
            object_wait_scheduler_invariant("prepared cancellation", error)
        }
    }

    /// Publishes the embedded scheduler generation while the caller holds its
    /// condition lock. Pairing the condition mutation with this call closes the
    /// check-to-park race.
    pub(crate) fn publish_locked(mut self, wait_queue: &WaitQueue) -> PublishedTimedWait {
        let registration = match self.registration.take() {
            Some(registration) => registration,
            None => object_wait_invariant("prepared wait published twice", None),
        };
        let park = match scheduler::prepare_registered_park_locked(wait_queue, registration) {
            Ok(park) => park,
            Err(error) => object_wait_scheduler_invariant("condition wait publication", error),
        };
        PublishedTimedWait {
            park: Some(park),
            timer: self.timer.take(),
        }
    }

    /// Cancels a preparation which could not publish its condition node.
    pub(crate) fn abort(mut self) -> Result<WaitOutcome, ObjectWaitError> {
        self.request_cancellation();
        let registration = match self.registration.take() {
            Some(registration) => registration,
            None => object_wait_invariant("prepared wait aborted twice", None),
        };
        let outcome = match scheduler::finish_wait(registration) {
            Ok(Some(outcome)) => outcome,
            Ok(None) => object_wait_invariant("cancelled preparation remained unresolved", None),
            Err(error) => object_wait_scheduler_invariant("prepared wait abort", error),
        };
        self.retire_timer(outcome)?;
        Ok(outcome)
    }

    fn take_registration(&mut self) -> WaitRegistration {
        match self.registration.take() {
            Some(registration) => registration,
            None => object_wait_invariant("prepared wait lost registration", None),
        }
    }

    fn retire_timer(&mut self, outcome: WaitOutcome) -> Result<(), ObjectWaitError> {
        let timer = match self.timer.take() {
            Some(timer) => timer,
            None => object_wait_invariant("prepared wait lost timer", Some(outcome)),
        };
        timer.retire(outcome)?;
        Ok(())
    }
}

impl Drop for PreparedTimedWait {
    fn drop(&mut self) {
        if self.registration.is_some() || self.timer.is_some() {
            hyper::debug::invariant_failure("object::wait::drop invariant")
        }
    }
}

impl PublishedTimedWait {
    /// True only when the current Thread must publish its condition node before
    /// handing the retained interrupt mask into the context switch.
    pub(crate) fn will_park(&self) -> bool {
        matches!(self.park, Some(scheduler::PrepareWait::Park(_)))
    }

    pub(crate) fn finish(
        mut self,
        interrupt_mask: InterruptMaskGuard<crate::hal::irq::LocalMask>,
    ) -> (WaitOutcome, Result<(), ObjectWaitError>) {
        let park = match self.park.take() {
            Some(park) => park,
            None => object_wait_invariant("published wait completed twice", None),
        };
        let outcome = park.retain_mask(interrupt_mask).complete();
        let timer = match self.timer.take() {
            Some(timer) => timer,
            None => object_wait_invariant("published wait lost timer", Some(outcome)),
        };
        let retirement = timer.retire(outcome).map_err(ObjectWaitError::from);
        (outcome, retirement)
    }
}

impl Drop for PublishedTimedWait {
    fn drop(&mut self) {
        if self.park.is_some() || self.timer.is_some() {
            hyper::debug::invariant_failure("object::wait::drop invariant")
        }
    }
}

/// Allocates, charges, and arms all resources needed to publish one timed
/// condition wait. Callers must prepare their condition-specific storage first.
pub(crate) fn prepare_timed_wait(
    domain: &ResourceDomain,
    deadline_nanoseconds: u64,
) -> Result<TimedWaitPreparation, ObjectWaitError> {
    let prepared_timer = match WaitDeadline::from_absolute_nanoseconds(deadline_nanoseconds)? {
        WaitDeadline::Elapsed => {
            return Ok(TimedWaitPreparation::Completed(WaitOutcome::TimedOut));
        }
        WaitDeadline::Infinite => PreparedWaitTimer::Infinite,
        WaitDeadline::At(deadline) => PreparedWaitTimer::try_finite(domain, deadline)?,
    };
    scheduler::ensure_sleepable().map_err(SignalWaitError::Scheduler)?;
    let registration =
        scheduler::begin_wait(WaitMobility::Migratable).map_err(SignalWaitError::Scheduler)?;
    let timer = match prepared_timer.arm(&registration) {
        Ok(timer) => timer,
        Err(error) => {
            finish_unpublished_wait(registration)?;
            return Err(error.into());
        }
    };
    Ok(TimedWaitPreparation::Armed(PreparedTimedWait {
        registration: Some(registration),
        timer: Some(timer),
    }))
}

impl ArmedWaitTimer {
    fn retire(self, outcome: WaitOutcome) -> Result<(), TimedWaitError> {
        match self {
            Self::Infinite => Ok(()),
            Self::Finite { timeout, _charge } => timeout.retire_after(outcome),
        }
    }
}

/// Executes one generation-qualified wait from local preparation to retirement.
pub(crate) fn wait_one(
    source: SignalSource<'_>,
    domain: &ResourceDomain,
    requested: u64,
    deadline_nanoseconds: u64,
    cancellation_requested: impl FnOnce() -> bool,
) -> Result<SignalWaitOutcome, ObjectWaitError> {
    let requested = source
        .validate(requested, false)
        .ok_or(ObjectWaitError::InvalidSignals)?;
    let signals = source.state();
    if let Some(snapshot) = signals.observe(requested) {
        return Ok(SignalWaitOutcome::Observed(snapshot));
    }

    let prepared_wait = prepare_signal_wait(domain, requested)?;

    let mut prepared = match prepare_timed_wait(domain, deadline_nanoseconds)? {
        TimedWaitPreparation::Completed(WaitOutcome::TimedOut) => {
            return Ok(SignalWaitOutcome::TimedOut);
        }
        TimedWaitPreparation::Completed(outcome) => {
            object_wait_invariant("unexpected immediate wait outcome", Some(outcome))
        }
        TimedWaitPreparation::Armed(prepared) => prepared,
    };

    // A cancellation preceding scheduler publication could not resolve this
    // ticket. Any later cancellation observes its exact Armed or Queued
    // generation under the scheduler lock.
    if cancellation_requested() {
        prepared.request_cancellation();
    }

    let outcome = match signals.wait_registered(prepared_wait, prepared.take_registration()) {
        Ok(outcome) => outcome,
        Err(error) => {
            prepared.retire_timer(WaitOutcome::Cancelled)?;
            return Err(error.into());
        }
    };
    prepared.retire_timer(scheduler_outcome(outcome))?;
    Ok(outcome)
}

/// Waits for the first ready member of an ordered, bounded object set.
///
/// Duplicate object identities share one signal-state registration. When its
/// notification wins, the lowest original item whose requested mask matches
/// the committed snapshot is reported.
pub(crate) fn wait_many(
    requests: &[SignalWaitRequest<'_>],
    domain: &ResourceDomain,
    deadline_nanoseconds: u64,
    cancellation_requested: impl FnOnce() -> bool,
) -> Result<SignalWaitManyOutcome, ObjectWaitError> {
    if requests.is_empty()
        || requests.len() > hyper::abi::native::HYPER_NATIVE_OBJECT_WAIT_MANY_MAX_ITEMS as usize
    {
        return Err(ObjectWaitError::InvalidSignals);
    }

    let scratch_bytes = requests
        .len()
        .checked_mul(
            core::mem::size_of::<ValidatedWait<'_>>()
                + core::mem::size_of::<CanonicalWait<'_>>()
                + core::mem::size_of::<Option<PreparedSignalWait>>(),
        )
        .ok_or(ObjectWaitError::AllocationSize)?;
    let _scratch_charge = domain
        .reserve(ResourceAmount::ZERO.with(
            ResourceKind::KernelMemoryBytes,
            allocation_bytes(scratch_bytes)?,
        ))?
        .commit();
    let mut validated = Vec::new();
    validated
        .try_reserve_exact(requests.len())
        .map_err(|_| SignalWaitError::Allocation)?;
    for request in requests {
        let requested = request
            .source
            .validate(request.requested, false)
            .ok_or(ObjectWaitError::InvalidSignals)?;
        validated.push(ValidatedWait {
            source: request.source,
            requested,
        });
    }

    for (index, request) in validated.iter().enumerate() {
        if let Some(snapshot) = request.source.state().observe(request.requested) {
            return Ok(SignalWaitManyOutcome::Observed { index, snapshot });
        }
    }

    let mut canonical = Vec::new();
    canonical
        .try_reserve_exact(requests.len())
        .map_err(|_| SignalWaitError::Allocation)?;
    for request in &validated {
        match canonical
            .iter_mut()
            .find(|candidate: &&mut CanonicalWait<'_>| candidate.source.same_state(request.source))
        {
            Some(candidate) => candidate.requested = candidate.requested.union(request.requested),
            None => canonical.push(CanonicalWait {
                source: request.source,
                requested: request.requested,
            }),
        }
    }

    let mut waiters = Vec::new();
    waiters
        .try_reserve_exact(canonical.len())
        .map_err(|_| SignalWaitError::Allocation)?;
    for request in &canonical {
        let charge = reserve_waiter(domain)?;
        waiters.push(Some(PreparedSignalWait::try_new(
            request.requested,
            charge,
        )?));
    }

    let mut prepared = match prepare_timed_wait(domain, deadline_nanoseconds)? {
        TimedWaitPreparation::Completed(WaitOutcome::TimedOut) => {
            return Ok(SignalWaitManyOutcome::TimedOut);
        }
        TimedWaitPreparation::Completed(outcome) => {
            object_wait_invariant("unexpected immediate multi-wait outcome", Some(outcome))
        }
        TimedWaitPreparation::Armed(prepared) => prepared,
    };
    if cancellation_requested() {
        prepared.request_cancellation();
    }
    let ticket = prepared.ticket();
    for (request, waiter) in canonical.iter().zip(waiters.iter_mut()) {
        let waiter = match waiter.take() {
            Some(waiter) => waiter,
            None => object_wait_invariant("multi-wait storage consumed twice", None),
        };
        request.source.state().register_shared_wait(waiter, ticket);
    }

    let scheduler_outcome = match canonical[0]
        .source
        .state()
        .park_shared_wait(prepared.take_registration())
    {
        Ok(outcome) => outcome,
        Err(error) => {
            for request in &canonical {
                let _ = request.source.state().unregister_shared_wait(ticket);
            }
            prepared.retire_timer(WaitOutcome::Cancelled)?;
            return Err(error.into());
        }
    };

    let mut winner = None;
    for request in &canonical {
        if let Some(snapshot) = request.source.state().unregister_shared_wait(ticket) {
            if winner.is_some() {
                object_wait_invariant("multi-wait selected more than one signal winner", None);
            }
            winner = Some((request.source, snapshot));
        }
    }
    prepared.retire_timer(scheduler_outcome)?;

    match (scheduler_outcome, winner) {
        (WaitOutcome::Notified, Some((source, snapshot))) => {
            let index = match validated.iter().position(|request| {
                request.source.same_state(source)
                    && snapshot.signals().intersects(request.requested)
            }) {
                Some(index) => index,
                None => object_wait_invariant("multi-wait winner matched no input item", None),
            };
            Ok(SignalWaitManyOutcome::Observed { index, snapshot })
        }
        (WaitOutcome::TimedOut, None) => Ok(SignalWaitManyOutcome::TimedOut),
        (WaitOutcome::Cancelled, None) => Ok(SignalWaitManyOutcome::Cancelled),
        _ => object_wait_invariant("multi-wait outcome disagreed with signal winner", None),
    }
}

// Retire accounting and by-value SignalWaiter construction scratch before
// arming timers or entering the scheduler. Only its owned allocation survives.
#[inline(never)]
fn prepare_signal_wait(
    domain: &ResourceDomain,
    requested: crate::object::signals::SignalMask,
) -> Result<PreparedSignalWait, ObjectWaitError> {
    let charge = reserve_waiter(domain)?;
    Ok(PreparedSignalWait::try_new(requested, charge)?)
}

fn reserve_waiter(domain: &ResourceDomain) -> Result<CommittedCharge, ObjectWaitError> {
    let bytes = allocation_bytes(PreparedSignalWait::allocation_size())?;
    Ok(domain
        .reserve(ResourceAmount::ZERO.with(ResourceKind::KernelMemoryBytes, bytes))?
        .commit())
}

fn allocation_bytes(bytes: usize) -> Result<u64, ObjectWaitError> {
    u64::try_from(bytes).map_err(|_| ObjectWaitError::AllocationSize)
}

fn finish_unpublished_wait(registration: WaitRegistration) -> Result<(), ObjectWaitError> {
    match scheduler::finish_wait(registration) {
        Ok(None | Some(WaitOutcome::Cancelled)) => Ok(()),
        Ok(Some(outcome)) => object_wait_invariant("unpublished wait resolved", Some(outcome)),
        Err(error) => Err(SignalWaitError::Scheduler(error).into()),
    }
}

const fn scheduler_outcome(outcome: SignalWaitOutcome) -> WaitOutcome {
    match outcome {
        SignalWaitOutcome::Observed(_) => WaitOutcome::Notified,
        SignalWaitOutcome::TimedOut => WaitOutcome::TimedOut,
        SignalWaitOutcome::Cancelled => WaitOutcome::Cancelled,
    }
}

#[cold]
fn object_wait_invariant(message: &str, outcome: Option<WaitOutcome>) -> ! {
    crate::diagnostics::crash::fatal(format_args!(
        "HypeR: object wait invariant failed: {message}; outcome={outcome:?}"
    ))
}

#[cold]
fn object_wait_scheduler_invariant(message: &str, error: scheduler::Error) -> ! {
    crate::diagnostics::crash::fatal(format_args!(
        "HypeR: object wait scheduler invariant failed: {message}: {error:?}"
    ))
}
