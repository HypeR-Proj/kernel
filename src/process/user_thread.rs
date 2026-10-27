// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! User-thread identity, join completion, and scheduler-owned execution.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicU64, Ordering};

use hyper::mm::{FallibleArc, WeakFallibleArc};
use hyper::sync::InterruptSpinLock;

use super::lifecycle::{LifecycleError, TerminalReason, UserThreadLifecycle, UserThreadPhase};
use super::owner::{Process, ProcessThreadMembership};
use crate::accounting::CommittedCharge;
use crate::authority::Rights;
use crate::mm::user_space::NativeAddressSpace;
use crate::sync::Completion;
use crate::task::thread::ThreadId;
use {crate::capability::HandleError, crate::capability::InvocationReceiver};
use {
    crate::object::KernelObject, crate::object::KernelRef, crate::object::KernelService,
    crate::object::ObjectCreationError, crate::object::ObjectKind,
    crate::object::ObjectPublication, crate::object::ObjectSnapshot, crate::object::OperationPin,
    crate::object::PublishableRef, crate::object::Scheduler as SchedulerReference,
    crate::object::SignalMask, crate::object::SignalSource, crate::object::SignalState,
    crate::object::TransferClass, crate::object::object_allocation_size, crate::object::private,
};

type ThreadLock<T> = InterruptSpinLock<T, crate::hal::irq::LocalMask>;

struct ThreadControl {
    lifecycle: UserThreadLifecycle,
    next_run_generation: u64,
    prepared_run: Option<RunIdentity>,
    active_run: Option<RunIdentity>,
}

struct UserThreadInner {
    process_id: super::owner::ProcessId,
    process: WeakFallibleArc<super::owner::ProcessInner>,
    scheduler_id: AtomicU64,
    control: ThreadLock<ThreadControl>,
    joined: Completion,
    signals: SignalState,
    _metadata_charge: CommittedCharge,
}

/// Erased object payload which gives one user thread a canonical KOID.
pub(super) struct UserThreadObject {
    inner: UserThreadInner,
}

impl private::Sealed for UserThreadObject {}
impl private::UserExportable for UserThreadObject {}

impl KernelObject for UserThreadObject {
    fn diagnostic_details(
        &self,
        cursor: u64,
    ) -> Result<crate::object::diagnostics::Details, crate::object::diagnostics::DetailError> {
        use {crate::object::diagnostics::DetailRecord, crate::object::diagnostics::Details};
        let phase = self
            .inner
            .control
            .with(|control| match control.lifecycle.phase() {
                UserThreadPhase::Prepared => 1,
                UserThreadPhase::Dormant => 2,
                UserThreadPhase::Runnable => 3,
                UserThreadPhase::StopRequested => 4,
                UserThreadPhase::Detached => 5,
            });
        Details::last(
            DetailRecord::Thread {
                tid: core::num::NonZeroU64::new(self.inner.scheduler_id.load(Ordering::Acquire))
                    .map(|id| id.get()),
                role: 4,
                phase,
            },
            cursor,
        )
    }

    const KIND: ObjectKind = ObjectKind::THREAD;
    const TRANSFER_CLASS: TransferClass = TransferClass::RendezvousOnly;
    // Thread capabilities transfer only by explicit rendezvous; scheduling
    // operations remain independently attenuable on the received handle.
    const SUPPORTED_RIGHTS: Rights = Rights::DUPLICATE
        .union(Rights::TRANSFER)
        .union(crate::task::scheduling_authority::SCHEDULING_RIGHTS)
        .union(Rights::WAIT)
        .union(Rights::INSPECT)
        .union(Rights::START)
        .union(Rights::REQUEST_STOP);

    fn on_zero_active_handles(&self, _: &mut crate::object::ObjectRetirement) {
        // Closing an unstarted thread must not retain Process membership forever.
        // Running threads may be detached; their execution remains an owner.
        let dormant = self
            .inner
            .control
            .with(|control| control.lifecycle.phase() == UserThreadPhase::Dormant);
        let raw = self.inner.scheduler_id.load(Ordering::Acquire);
        if dormant && raw != 0 {
            let _ = crate::task::scheduler::request_user_thread_stop(
                ThreadId::from_process_publication(raw),
                TerminalReason::Requested,
            );
        }
    }

    fn signal_source(&self) -> Option<SignalSource<'_>> {
        Some(SignalSource::new(
            &self.inner.signals,
            UserThread::SUPPORTED_SIGNALS,
        ))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RunIdentity {
    generation: u64,
    image_generation: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RunAdmissionError {
    AdmissionClosed,
    AlreadyAdmitted,
    GenerationExhausted,
    StaleImage,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct UserThreadSnapshot {
    pub(crate) scheduler_id: Option<ThreadId>,
    pub(crate) phase: UserThreadPhase,
    pub(crate) terminal: Option<TerminalReason>,
}

/// Observer and operation owner which outlives scheduler reclamation.
///
/// The object keeps only a weak Process relationship. Scheduler execution owns
/// the strong Process membership, so a Process handle table may retain this
/// object without forming a Process-to-Thread-to-Process reference cycle.
pub(crate) struct UserThread {
    object: UserThreadOwner,
}

enum UserThreadOwner {
    Service(PublishableRef<UserThreadObject, KernelService>),
    Scheduler(KernelRef<UserThreadObject, SchedulerReference>),
    Operation(KernelRef<UserThreadObject, OperationPin>),
}

impl UserThread {
    pub(crate) const TERMINATED: SignalMask =
        SignalMask::from_trusted_bits(hyper::abi::native::HYPER_NATIVE_SIGNAL_THREAD_TERMINATED);
    pub(crate) const SUPPORTED_SIGNALS: SignalMask = Self::TERMINATED;

    pub(super) const fn allocation_size() -> Option<usize> {
        object_allocation_size::<UserThreadObject>()
    }

    pub(super) fn try_prepared(
        process: &Process,
        metadata_charge: CommittedCharge,
    ) -> Result<Self, ObjectCreationError> {
        let inner = UserThreadInner {
            process_id: process.id(),
            process: process.inner.downgrade(),
            scheduler_id: AtomicU64::new(0),
            control: ThreadLock::new(ThreadControl {
                lifecycle: UserThreadLifecycle::prepared(),
                next_run_generation: 1,
                prepared_run: None,
                active_run: None,
            }),
            joined: Completion::new(),
            signals: SignalState::new(),
            _metadata_charge: metadata_charge,
        };
        let object = PublishableRef::try_new(UserThreadObject { inner })?;
        Ok(Self {
            object: UserThreadOwner::Service(object),
        })
    }

    fn inner(&self) -> &UserThreadInner {
        match &self.object {
            UserThreadOwner::Service(object) => &object.object().inner,
            UserThreadOwner::Scheduler(object) => &object.object().inner,
            UserThreadOwner::Operation(object) => &object.object().inner,
        }
    }

    pub(super) fn publication(&self) -> ObjectPublication<UserThreadObject> {
        match &self.object {
            UserThreadOwner::Service(object) => object.publication(),
            UserThreadOwner::Scheduler(_) | UserThreadOwner::Operation(_) => {
                hyper::debug::invariant_failure("process::user_thread::publication invariant")
            }
        }
    }

    /// Reads the immutable grant while briefly retaining the owning process.
    pub(crate) fn scheduling_limits(
        &self,
    ) -> Option<crate::task::scheduling_authority::SchedulingLimits> {
        self.inner()
            .process
            .upgrade()
            .map(|inner| super::owner::Process { inner }.scheduling().limits)
    }

    /// Uses captured invocation authority without repeating handle-table lookup.
    pub(crate) fn resolve_receiver(
        receiver: &InvocationReceiver,
        rights: Rights,
    ) -> Result<Self, HandleError> {
        let resolved = receiver.resolve::<UserThreadObject>(rights)?;
        Ok(Self {
            object: UserThreadOwner::Operation(resolved.into_operation_pin()),
        })
    }

    pub(crate) fn into_scheduler_owner(self) -> Self {
        match self.object {
            UserThreadOwner::Service(object) => Self {
                object: UserThreadOwner::Scheduler(object.into_scheduler()),
            },
            UserThreadOwner::Scheduler(_) => hyper::debug::invariant_failure(
                "process::user_thread::into_scheduler_owner invariant",
            ),
            UserThreadOwner::Operation(_) => hyper::debug::invariant_failure(
                "process::user_thread::into_scheduler_owner invariant",
            ),
        }
    }

    pub(crate) fn object_snapshot(&self) -> ObjectSnapshot {
        match &self.object {
            UserThreadOwner::Service(object) => object.snapshot(),
            UserThreadOwner::Scheduler(object) => object.snapshot(),
            UserThreadOwner::Operation(object) => object.snapshot(),
        }
    }

    pub(crate) fn process_id(&self) -> super::owner::ProcessId {
        self.inner().process_id
    }

    fn process(&self) -> Option<Process> {
        self.inner()
            .process
            .upgrade()
            .map(|inner| Process { inner })
    }

    pub(crate) fn scheduler_id(&self) -> Option<ThreadId> {
        let raw = self.inner().scheduler_id.load(Ordering::Acquire);
        (raw != 0).then(|| ThreadId::from_process_publication(raw))
    }

    pub(crate) fn snapshot(&self) -> UserThreadSnapshot {
        self.inner().control.with(|control| UserThreadSnapshot {
            scheduler_id: self.scheduler_id(),
            phase: control.lifecycle.phase(),
            terminal: control.lifecycle.terminal(),
        })
    }

    pub(crate) fn stop_requested(&self) -> bool {
        self.inner()
            .control
            .with(|control| control.lifecycle.phase() == UserThreadPhase::StopRequested)
    }

    pub(crate) fn request_stop(&self, reason: TerminalReason) -> bool {
        self.inner()
            .control
            .with(|control| control.lifecycle.request_terminal(reason))
    }

    pub(crate) fn ready(&self) -> Result<bool, super::owner::ProcessError> {
        let id = self
            .scheduler_id()
            .ok_or(super::owner::ProcessError::Lifecycle(
                LifecycleError::AdmissionClosed,
            ))?;
        let process = self.process().ok_or(super::owner::ProcessError::Lifecycle(
            LifecycleError::AdmissionClosed,
        ))?;
        process
            .with_run_admission(process.image_generation(), || {
                crate::task::scheduler::ready_user_thread(id)
            })
            .map_err(|_| super::owner::ProcessError::Lifecycle(LifecycleError::AdmissionClosed))?
            .map_err(|error| match error {
                crate::task::scheduler::Error::ThreadNotFound
                | crate::task::scheduler::Error::TerminatedThread
                | crate::task::scheduler::Error::InvalidThreadState => {
                    super::owner::ProcessError::Lifecycle(LifecycleError::AdmissionClosed)
                }
                other => other.into(),
            })
    }

    pub(crate) fn mark_runnable(&self) -> Result<(), LifecycleError> {
        self.inner()
            .control
            .with(|control| control.lifecycle.mark_runnable())
    }

    /// Reserves one logical architecture generation for this Thread.
    ///
    /// The generation follows its syscall continuation across scheduling and
    /// migration. CPU-local hardware ownership belongs to entry policy.
    pub(crate) fn prepare_run(
        &self,
        expected_image_generation: u64,
    ) -> Result<PreparedUserRun, RunAdmissionError> {
        let Some(process) = self.process() else {
            return Err(RunAdmissionError::AdmissionClosed);
        };
        let identity = process.with_run_admission(expected_image_generation, || {
            self.inner().control.with(|control| {
                if control.lifecycle.phase() != UserThreadPhase::Runnable {
                    return Err(RunAdmissionError::AdmissionClosed);
                }
                if control.prepared_run.is_some() || control.active_run.is_some() {
                    return Err(RunAdmissionError::AlreadyAdmitted);
                }
                let generation = control.next_run_generation;
                control.next_run_generation = generation
                    .checked_add(1)
                    .ok_or(RunAdmissionError::GenerationExhausted)?;
                let identity = RunIdentity {
                    generation,
                    image_generation: expected_image_generation,
                };
                control.prepared_run = Some(identity);
                Ok(identity)
            })
        })??;
        Ok(PreparedUserRun {
            thread: self.clone(),
            identity,
            armed: true,
        })
    }

    #[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
    #[allow(
        dead_code,
        reason = "Join helpers are consumed by architecture-specific Native self-tests"
    )]
    pub(crate) fn join(&self) -> Result<TerminalReason, crate::sync::Error> {
        self.inner().joined.wait()?;
        match self.snapshot().terminal {
            Some(reason) => Ok(reason),
            None => hyper::debug::invariant_failure("process::user_thread::join invariant"),
        }
    }

    #[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
    #[allow(
        dead_code,
        reason = "Join helpers are consumed by architecture-specific Native self-tests"
    )]
    pub(crate) fn try_join(&self) -> Option<TerminalReason> {
        if !self.inner().joined.try_wait() {
            return None;
        }
        match self.snapshot().terminal {
            Some(reason) => Some(reason),
            None => hyper::debug::invariant_failure("process::user_thread::try_join invariant"),
        }
    }

    pub(super) fn publish(&self, id: ThreadId) {
        if id == ThreadId::BOOTSTRAP || self.inner().scheduler_id.load(Ordering::Relaxed) != 0 {
            hyper::debug::invariant_failure("process::user_thread::publish invariant");
        }
        self.inner().control.with(|control| {
            if control.lifecycle.publish().is_err() {
                hyper::debug::invariant_failure("process::user_thread::publish invariant");
            }
        });
        // The ID is the publication word for every control field initialized
        // above. Acquire observers of a nonzero ID see complete ownership.
        self.inner().scheduler_id.store(id.get(), Ordering::Release);
    }

    fn publish_terminal_state(&self) -> TerminalReason {
        let terminal = self.inner().control.with(|control| {
            if control.prepared_run.is_some() || control.active_run.is_some() {
                hyper::debug::invariant_failure(
                    "process::user_thread::publish_terminal_state invariant",
                );
            }
            match control.lifecycle.detach() {
                Ok(terminal) => terminal,
                Err(_) => hyper::debug::invariant_failure(
                    "process::user_thread::publish_terminal_state invariant",
                ),
            }
        });
        // Publish the object-visible terminal state before Process membership
        // can complete the enclosing Process. A waiter which observes Process
        // completion must never find one of its detached Thread objects still
        // missing TERMINATED.
        if self
            .inner()
            .signals
            .update(SignalMask::EMPTY, Self::TERMINATED)
            .is_err()
        {
            hyper::debug::invariant_failure(
                "process::user_thread::publish_terminal_state invariant",
            );
        }
        terminal
    }

    fn publish_join_completion(&self) {
        if self.inner().joined.complete_all().is_err() {
            hyper::debug::invariant_failure(
                "process::user_thread::publish_join_completion invariant",
            );
        }
    }
}

impl Clone for UserThread {
    fn clone(&self) -> Self {
        Self {
            object: match &self.object {
                UserThreadOwner::Service(object) => UserThreadOwner::Service(object.clone()),
                UserThreadOwner::Scheduler(object) => UserThreadOwner::Scheduler(object.clone()),
                UserThreadOwner::Operation(object) => UserThreadOwner::Operation(object.clone()),
            },
        }
    }
}

/// Linear Process ownership installed into one dormant scheduler execution.
///
/// Scheduler code transports this token but cannot inspect its Process-local
/// membership or resource accounting internals.
pub(crate) struct UserExecutionOwnership {
    membership: ProcessThreadMembership,
    execution_charge: CommittedCharge,
}

impl UserExecutionOwnership {
    pub(super) const fn new(
        membership: ProcessThreadMembership,
        execution_charge: CommittedCharge,
    ) -> Self {
        Self {
            membership,
            execution_charge,
        }
    }
}

/// Scheduler-owned payload for a runnable native user context.
///
/// This payload owns the strong Process membership while execution is resident
/// in the scheduler. There is no reverse owning edge from Process bookkeeping
/// to the scheduler Thread. The address-space clone is dropped before Process
/// membership completion, so observing zero active Process Threads also proves
/// that no scheduler payload can retain the machine root.
pub(crate) struct UserExecution {
    ownership: Option<UserExecutionOwnership>,
    address_space: Option<FallibleArc<NativeAddressSpace>>,
    context: UnsafeCell<crate::hal::user::UserContext>,
    armed: bool,
}

impl UserExecution {
    pub(super) const fn allocation_size() -> usize {
        core::mem::size_of::<Self>()
    }

    // Do not merge register-image allocation scratch into callers which also
    // retain the Result returned by architectural context preparation.
    #[inline(never)]
    pub(super) fn try_new(
        address_space: FallibleArc<NativeAddressSpace>,
        context: crate::hal::user::UserContext,
    ) -> Result<alloc::boxed::Box<UnsafeCell<Self>>, ()> {
        hyper::mm::try_box(UnsafeCell::new(Self {
            ownership: None,
            address_space: Some(address_space),
            context: UnsafeCell::new(context),
            armed: false,
        }))
        .map_err(|_| ())
    }

    pub(crate) fn address_space(&self) -> &NativeAddressSpace {
        match self.address_space.as_ref() {
            Some(address_space) => address_space,
            None => {
                hyper::debug::invariant_failure("process::user_thread::address_space invariant")
            }
        }
    }

    pub(crate) fn process(&self) -> &Process {
        match self.ownership.as_ref() {
            Some(ownership) => ownership.membership.process(),
            None => hyper::debug::invariant_failure("process::user_thread::process invariant"),
        }
    }

    /// Returns the register owner to the fixed native-user trampoline.
    ///
    /// # Safety
    ///
    /// The caller must execute this payload's scheduler Thread and own one
    /// admitted generation. Only its architecture entry and switch-detach
    /// mechanism may access the context until return ownership is consumed.
    /// Scheduling suspends this continuation without releasing its payload.
    pub(crate) unsafe fn context_ptr(&self) -> *mut crate::hal::user::UserContext {
        self.context.get()
    }

    pub(crate) fn arm_for_process_publication(&mut self, ownership: UserExecutionOwnership) {
        if self.armed || self.ownership.is_some() {
            hyper::debug::invariant_failure(
                "process::user_thread::arm_for_process_publication invariant",
            );
        }
        self.ownership = Some(ownership);
        self.armed = true;
    }

    /// Completes scheduler-detach ownership outside the scheduler lock.
    pub(crate) fn complete_detach(mut self, thread: UserThread) {
        if !self.armed {
            hyper::debug::invariant_failure("process::user_thread::complete_detach invariant");
        }
        self.armed = false;
        drop(self.address_space.take());
        let terminal = thread.publish_terminal_state();
        let ownership = match self.ownership.take() {
            Some(ownership) => ownership,
            None => {
                hyper::debug::invariant_failure("process::user_thread::complete_detach invariant")
            }
        };
        // The scheduler and machine publication have detached; FP state is
        // already saved and scrubbed. Only the owned register image is copied.
        ownership.membership.detach(
            terminal,
            thread.object_snapshot().koid.get(),
            self.context.get_mut(),
        );
        drop(ownership.execution_charge);
        thread.publish_join_completion();
    }
}

impl Drop for UserExecution {
    fn drop(&mut self) {
        if self.armed || self.ownership.is_some() {
            // Reclamation must call complete_detach after releasing scheduler
            // ownership. Drop cannot acquire Process/completion lock graphs.
            hyper::debug::invariant_failure("process::user_thread::drop invariant");
        }
    }
}

#[must_use = "publish or abort the prepared user run"]
pub(crate) struct PreparedUserRun {
    thread: UserThread,
    identity: RunIdentity,
    armed: bool,
}

impl PreparedUserRun {
    /// Publishes one generation after the initial machine preparation succeeds.
    pub(crate) fn commit(mut self) -> Result<ActiveUserRun, RunAdmissionError> {
        let Some(process) = self.thread.process() else {
            self.abort_inner();
            return Err(RunAdmissionError::AdmissionClosed);
        };
        let publication = process.with_run_admission(self.identity.image_generation, || {
            self.thread.inner().control.with(|control| {
                if control.lifecycle.phase() != UserThreadPhase::Runnable {
                    return Err(RunAdmissionError::AdmissionClosed);
                }
                if control.prepared_run != Some(self.identity) || control.active_run.is_some() {
                    return Err(RunAdmissionError::AlreadyAdmitted);
                }
                control.prepared_run = None;
                control.active_run = Some(self.identity);
                Ok(())
            })
        });
        if let Err(error) | Ok(Err(error)) = publication {
            self.abort_inner();
            return Err(error);
        }
        self.armed = false;
        Ok(ActiveUserRun {
            thread: self.thread.clone(),
            identity: self.identity,
            armed: true,
        })
    }

    pub(crate) fn abort(mut self) {
        self.abort_inner();
    }

    fn abort_inner(&mut self) {
        self.thread.inner().control.with(|control| {
            if control.prepared_run != Some(self.identity) {
                hyper::debug::invariant_failure("process::user_thread::abort_inner invariant");
            }
            control.prepared_run = None;
        });
        self.armed = false;
    }
}

impl Drop for PreparedUserRun {
    fn drop(&mut self) {
        if self.armed {
            hyper::debug::invariant_failure("process::user_thread::drop invariant");
        }
    }
}

#[must_use = "complete the active user run before scheduler detach"]
pub(crate) struct ActiveUserRun {
    thread: UserThread,
    identity: RunIdentity,
    armed: bool,
}

impl ActiveUserRun {
    pub(crate) fn binding(&self) -> hyper::hal::user::UserRunBinding {
        match hyper::hal::user::UserRunBinding::new(
            self.thread.scheduler_id().map_or(0, ThreadId::get),
            self.identity.image_generation,
            self.identity.generation,
        ) {
            Some(binding) => binding,
            None => hyper::debug::invariant_failure("process::user_thread::binding invariant"),
        }
    }

    /// Closes machine ownership after publication and translation are idle.
    ///
    /// The durable generation remains occupied until the architecture return
    /// capability has been consumed, independently of the CPU which ran it.
    pub(crate) fn stop_after_machine_exit(
        mut self,
        proof: crate::mm::user_space::StoppedNativeRun,
    ) -> StoppedUserRun {
        if proof.binding() != self.binding() {
            hyper::debug::invariant_failure(
                "process::user_thread::stop_after_machine_exit invariant",
            );
        }
        self.armed = false;
        StoppedUserRun {
            thread: self.thread.clone(),
            identity: self.identity,
            armed: true,
        }
    }
}

impl Drop for ActiveUserRun {
    fn drop(&mut self) {
        if self.armed {
            hyper::debug::invariant_failure("process::user_thread::drop invariant");
        }
    }
}

/// Durable ownership of one stopped architecture generation.
///
/// This token is deliberately not CPU-affine. Fault resolution may block and
/// resume on another CPU, but architecture return ownership must be resolved before
/// this logical generation is acknowledged.
#[must_use = "stopped user generation must follow architecture completion"]
pub(crate) struct StoppedUserRun {
    thread: UserThread,
    identity: RunIdentity,
    armed: bool,
}

impl StoppedUserRun {
    /// Acknowledges that the architecture return capability was consumed.
    pub(crate) fn acknowledge_architecture_exit(mut self) {
        self.thread.inner().control.with(|control| {
            if control.active_run != Some(self.identity) {
                hyper::debug::invariant_failure(
                    "process::user_thread::acknowledge_architecture_exit invariant",
                );
            }
            control.active_run = None;
        });
        self.armed = false;
    }
}

impl Drop for StoppedUserRun {
    fn drop(&mut self) {
        if self.armed {
            hyper::debug::invariant_failure("process::user_thread::drop invariant");
        }
    }
}
