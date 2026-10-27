// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Public scheduler operations and architecture context-switch boundary.

pub(super) mod algorithms;
mod balance;
mod balance_policy;
mod configuration;
#[cfg(not(test))]
mod cpu_time;
mod queue;
mod registry;
#[cfg(all(feature = "kernel-test", feature = "kernel-stack-audit"))]
mod stack_audit;
mod state;
mod switch_handoff;
mod trace;

#[cfg(not(test))]
use cpu_time::account_cpu_time;
#[cfg(not(test))]
pub(crate) use cpu_time::{CpuTimeSnapshot, snapshot as cpu_time_snapshot};

#[cfg(test)]
pub(super) fn account_cpu_time<T>(_cpu: hyper::cpu::CpuIndex, _role: T, _elapsed: u64) {}

use alloc::boxed::Box;
use alloc::rc::Rc;
use core::marker::PhantomData;
use core::ptr::NonNull;

use hyper::cpu::CpuIndex;
use hyper::sync::atomic::{AtomicUsize, Ordering};
use hyper::sync::{InterruptMaskGuard, InterruptShardedLock};

use self::registry::ThreadReservation;
use self::state::{PreparedContextSwitch, Scheduler};
use super::preempt::RescheduleSource;
pub use super::thread::ThreadNameSnapshot;
use super::thread::{
    ExternalThreadExecution, KernelThreadEntry, Thread, ThreadId, ThreadResourceOwnership,
    ThreadState,
};
use trace::SchedulePoint;

use super::policy;
pub use super::policy::{CpuMask, ThreadPriority};
use super::policy::{SchedulingParameters, SchedulingPolicy, ThreadPlacement};
use super::wait::{WaitMobility, WaitOutcome, WaitQueue, WaitTicket};
pub(crate) use configuration::default_parameters;

use crate::pr_debug;

#[cfg(all(
    target_arch = "aarch64",
    feature = "kernel-test",
    feature = "kernel-self-test"
))]
pub(crate) use balance::idle_for_test as balance_idle_for_test;
pub(crate) use balance::initialize as initialize_load_balancing;
pub(crate) use balance::service_irq_prompt as service_balance_irq_prompt;

type CoordinatorLock =
    InterruptShardedLock<Option<Scheduler>, crate::hal::irq::LocalMask, { hyper::cpu::MAX_CPUS }>;
type TransitionMask = InterruptMaskGuard<crate::hal::irq::LocalMask>;
type TransitionMaskState =
    <crate::hal::irq::LocalMask as hyper::hal::interrupt::InterruptMask>::State;

static SCHEDULER: CoordinatorLock =
    InterruptShardedLock::new_notifying(None, balance::coordinator_released);

/// Accesses Thread-owned wait resources without exposing registry pointers.
/// The callback must not re-enter the scheduler or acquire a condition bucket.
pub(crate) fn with_wait_context<R>(
    id: ThreadId,
    operation: impl FnOnce(&super::ThreadWaitContext) -> R,
) -> Result<R, Error> {
    read_scheduler(|scheduler| scheduler.with_thread(id, |thread| operation(thread.wait_context())))
}

/// Negative test of the real exit gate with an idle scheduler wait record and
/// a still-linked condition node. Unexpected success cannot be rolled back.
#[cfg(all(
    all(feature = "kernel-test", feature = "kernel-self-test"),
    CONFIG_ARCH_AARCH64
))]
pub(crate) fn verify_wait_exit_rejected() -> Result<bool, Error> {
    let cpu = current_cpu()?;
    SCHEDULER.with(|slot| {
        let scheduler = slot.as_mut().ok_or(Error::NotInitialized)?;
        match scheduler.prepare_exit(cpu) {
            Err(Error::InvalidWaitRegistration) => Ok(true),
            Err(error) => Err(error),
            Ok(_) => hyper::debug::invariant_failure("live wait resources passed Thread exit gate"),
        }
    })
}

/// Pins registry lifetime and placement without serializing other CPUs.
/// No reader may upgrade to exclusive coordination or enter another reader.
fn read_scheduler<R>(operation: impl FnOnce(&Scheduler) -> Result<R, Error>) -> Result<R, Error> {
    // SAFETY: masks remain lexical; choosing the reader lane and using it
    // happen on the same CPU, before any possible rescheduling.
    let mask = unsafe { TransitionMask::acquire() };
    let cpu = current_cpu()?;
    let result = SCHEDULER
        .read(cpu.get(), |slot| {
            operation(slot.as_ref().ok_or(Error::NotInitialized)?)
        })
        .ok_or(Error::InvalidCpuIndex)?;
    drop(mask);
    result
}

/// Holds one reader lane for the SMP progress proof. The callback must not
/// enter scheduler APIs or retain the IRQ mask beyond this call.
#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
pub(crate) fn hold_reader_for_test(operation: impl FnOnce()) -> Result<(), Error> {
    read_scheduler(|_scheduler| {
        operation();
        Ok(())
    })
}
static RETIREMENTS_IN_PROGRESS: AtomicUsize = AtomicUsize::new(0);

/// Locks scheduler-detached resources into the observable retirement epoch.
///
/// Construction occurs under `SCHEDULER` immediately after detachment. Drop
/// release-publishes that every lock-external resource destructor and terminal
/// publication owned by the detached Thread has completed.
pub(super) struct ResourceRetirement {
    _private: (),
}

impl ResourceRetirement {
    fn begin() -> Self {
        if RETIREMENTS_IN_PROGRESS
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |count| {
                count.checked_add(1)
            })
            .is_err()
        {
            hyper::debug::invariant_failure("task::scheduler::mod::begin invariant");
        }
        Self { _private: () }
    }
}

impl Drop for ResourceRetirement {
    fn drop(&mut self) {
        if RETIREMENTS_IN_PROGRESS
            .fetch_update(Ordering::Release, Ordering::Relaxed, |count| {
                count.checked_sub(1)
            })
            .is_err()
        {
            hyper::debug::invariant_failure("task::scheduler::mod::drop invariant");
        }
    }
}

/// Scheduler ownership of one prepared, non-runnable vCPU thread.
///
/// Dropping the capability before installation removes the dormant thread.
/// The VM installation transaction consumes it after publishing the aggregate;
/// no safe API exposes the reserved `ThreadId` before that point.
/// A rollback invariant violation is fail-stop because continuing could leave
/// a scheduler-owned raw binding to an allocation about to be released.
pub(crate) struct DormantVcpuThread {
    thread: ThreadId,
    rollback: bool,
}

/// Scheduler ownership of one non-runnable, unpublished user Thread.
pub(crate) struct DormantUserThread {
    thread: ThreadId,
    rollback: bool,
}

impl DormantUserThread {
    pub(crate) const fn id(&self) -> ThreadId {
        self.thread
    }

    /// Arms explicit reaper retirement and ends rollback before Process
    /// membership performs its final publication.
    pub(crate) fn commit_before_process_publication(
        mut self,
        ownership: crate::process::UserExecutionOwnership,
    ) {
        let result = SCHEDULER.with(|slot| {
            slot.as_mut()
                .ok_or(Error::NotInitialized)?
                .arm_dormant_user(self.thread, ownership)
        });
        if result.is_err() {
            hyper::debug::invariant_failure(
                "task::scheduler::mod::commit_before_process_publication invariant",
            );
        }
        self.rollback = false;
    }
}

impl Drop for DormantUserThread {
    fn drop(&mut self) {
        if !self.rollback {
            return;
        }
        let thread = match SCHEDULER.with(|slot| {
            slot.as_mut()
                .ok_or(Error::NotInitialized)?
                .take_dormant_user(self.thread)
        }) {
            Ok(thread) => thread,
            Err(_) => hyper::debug::invariant_failure("task::scheduler::mod::drop invariant"),
        };
        drop(thread);
    }
}

impl DormantVcpuThread {
    /// Returns the reserved identity to the VM installation transaction.
    ///
    /// # Safety
    ///
    /// The caller must not expose this identity to `thread_ready` before the
    /// VM aggregate backing this thread's binding has been installed.
    pub(crate) const unsafe fn id_for_vm_install(&self) -> ThreadId {
        self.thread
    }

    /// Transfers the installed dormant thread to scheduler ownership.
    ///
    /// # Safety
    ///
    /// The VM aggregate backing this thread's binding must already occupy its
    /// permanent registry slot.
    pub(crate) unsafe fn commit_after_vm_install(mut self) -> ThreadId {
        self.rollback = false;
        self.thread
    }
}

impl Drop for DormantVcpuThread {
    fn drop(&mut self) {
        if !self.rollback {
            return;
        }
        let thread = match SCHEDULER.with(|slot| {
            slot.as_mut()
                .ok_or(Error::NotInitialized)?
                .take_dormant_vcpu(self.thread)
        }) {
            Ok(thread) => thread,
            Err(error) => {
                // Continuing would let the VM transaction release the
                // allocation referenced by this scheduler-owned Thread. This
                // is a soundness boundary, so a violated rollback invariant is
                // fatal in release builds as well as debug builds. Drop can
                // run under arbitrary locks, so use only crash-safe diagnostics.
                let _ = error;
                hyper::debug::invariant_failure("task::scheduler::mod::drop invariant")
            }
        };
        // Drop the stack, architecture context, and raw VM binding only after
        // releasing the scheduler lock; their allocators have independent
        // locks and the enclosing VM transaction is still alive.
        drop(thread);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    InvalidSchedulingParameters,
    SchedulingPermission(super::scheduling_authority::SchedulingPermissionError),
    AlreadyInitialized,
    NotInitialized,
    Allocation,
    /// Internal retry: allocate the missing stable table segment outside locks.
    ThreadTableGrowth,
    IdentifierExhausted,
    CurrentThreadMissing,
    ThreadNotFound,
    TerminatedThread,
    ThreadBlocked,
    ThreadAlreadyQueued,
    QueueCorrupted,
    CannotBlockIdle,
    CannotSleepWithInterruptsMasked,
    CannotSleepWithPreemptionDisabled,
    IrqTailRequiresInterruptsMasked,
    InvalidThreadState,
    IdleThreadAlreadyInstalled,
    InvalidIdleTransition,
    CpuAlreadyRegistered,
    CpuNotRegistered,
    InvalidCpuIndex,
    EmptyCpuAffinity,
    NoRegisteredCpuInAffinity,
    CpuNotAllowed,
    MigrationUnsupported,
    MigrationInProgress,
    ThreadTransitionInProgress,
    WaitGenerationExhausted,
    InvalidWaitRegistration,
    MigrationBlockedByCpuLocalWait,
    PreemptionUnavailable,
    PreemptionInvariant,
    VmEntryUnavailable,
    Thread(super::thread::Error),
}

impl From<super::preempt::Error> for Error {
    fn from(error: super::preempt::Error) -> Self {
        match error {
            super::preempt::Error::Offline | super::preempt::Error::InvalidCpu => {
                Self::PreemptionUnavailable
            }
            super::preempt::Error::AlreadyOnline
            | super::preempt::Error::DisableDepthOverflow
            | super::preempt::Error::DisableDepthUnderflow
            | super::preempt::Error::IrqDepthOverflow
            | super::preempt::Error::IrqDepthUnderflow
            | super::preempt::Error::WrongCpu => Self::PreemptionInvariant,
        }
    }
}

impl From<super::thread::Error> for Error {
    fn from(error: super::thread::Error) -> Self {
        Self::Thread(error)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Capabilities {
    pub bootstrap_thread: ThreadId,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SecondaryStack {
    pub physical_top: u64,
    pub virtual_top: usize,
}

#[derive(Clone, Copy)]
pub(crate) struct CurrentVcpu {
    pub thread: ThreadId,
    pub execution: super::external_execution::ExternalExecutionPointer,
    pub stack: (usize, usize),
}

pub(crate) struct CurrentUser {
    pub thread: ThreadId,
    pub object: crate::process::UserThread,
    pub execution: NonNull<crate::process::UserExecution>,
    pub stack: (usize, usize),
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Statistics {
    pub threads: usize,
    pub real_time_class_threads: usize,
    pub fair_class_threads: usize,
    pub budget_class_threads: usize,
    pub idle_class_threads: usize,
    pub ready: usize,
    pub running: usize,
    pub blocked: usize,
    pub migrating: usize,
    pub idle: usize,
    /// Threads detached from scheduler state whose owned resources are not yet retired.
    pub retirements_in_progress: usize,
    pub context_switches: u64,
    pub per_cpu_ready: [usize; hyper::config::MAX_CPUS as usize],
}

/// Completion state of an explicit scheduler placement change.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MigrationStatus {
    /// Assignment and any ready-queue membership already moved to the target.
    Completed,
    /// Asynchronous source-context handoff was accepted.
    ///
    /// IRQ-tail preemption may complete the handoff before the requesting call
    /// returns. This status is therefore not a completion token; callers which
    /// need observation must query placement or use a higher-level handshake.
    Pending,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CrashTaskSnapshot {
    pub id: ThreadId,
    pub state: ThreadState,
    pub execution: super::thread::ExecutionKind,
    pub stack: Option<(usize, usize)>,
    pub stack_statistics: Option<crate::mm::stack::StackStatistics>,
}

#[must_use = "a committed park must retain its IRQ mask through context handoff"]
pub(crate) struct ParkCommit {
    switch: PreparedContextSwitch,
    ticket: WaitTicket,
}

#[must_use = "parking is committed only after the prepared context switch is consumed"]
pub(crate) struct ParkToken {
    transition: PreparedTransition,
    ticket: WaitTicket,
}

/// Armed current-Thread wait which must be finished or committed to a park.
#[must_use = "an armed wait registration must be finished or parked"]
pub(crate) struct WaitRegistration {
    ticket: WaitTicket,
    active: bool,
    // Registrations belong to the Thread continuation which armed them.
    not_send_or_sync: PhantomData<Rc<()>>,
}

impl WaitRegistration {
    pub(crate) const fn ticket(&self) -> WaitTicket {
        self.ticket
    }

    fn disarm(&mut self) {
        self.active = false;
    }
}

impl Drop for WaitRegistration {
    fn drop(&mut self) {
        if self.active {
            // This linear owner can be abandoned from arbitrary lock/IRQ
            // context. Report through the panic handler while retaining the
            // registered wait; do not attempt ordinary cleanup or logging.
            hyper::debug::invariant_failure("task::scheduler::mod::drop invariant")
        }
    }
}

pub(crate) enum PrepareWait {
    Park(ParkCommit),
    Completed(WaitOutcome),
}

pub(crate) enum PreparedWait {
    Park(ParkToken),
    Completed(WaitOutcome),
}

impl PrepareWait {
    /// Transfers the retained IRQ mask to the switch, or restores it when the
    /// wait already completed without parking.
    pub(crate) fn retain_mask(self, interrupt_mask: TransitionMask) -> PreparedWait {
        match self {
            Self::Park(commit) => PreparedWait::Park(retain_park_mask(commit, interrupt_mask)),
            Self::Completed(outcome) => {
                drop(interrupt_mask);
                PreparedWait::Completed(outcome)
            }
        }
    }
}

impl PreparedWait {
    pub(crate) fn complete(self) -> WaitOutcome {
        match self {
            Self::Park(token) => complete_park(token),
            Self::Completed(outcome) => outcome,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ResolveWait {
    pub won: bool,
    pub made_ready: bool,
}

struct PreparedTransition {
    switch: PreparedContextSwitch,
    interrupt_mask: TransitionMask,
}

impl PreparedTransition {
    fn activate(self) {
        let Self {
            switch,
            interrupt_mask,
        } = self;
        // Selection is committed and scheduler locks are released. Retire the
        // outgoing Native CPU ownership before assembly can publish its saved
        // continuation to another CPU; scheduler `current` already names the
        // incoming Thread, so the entry hook uses its own CPU-local owner.
        if let Err(error) = service_deadline_publication() {
            scheduler_invariant("scheduler deadline publication", error);
        }
        crate::entry::user::suspend_current();
        // SAFETY: The architecture switch stores this exact state into the
        // outgoing ThreadContext before changing stacks. Its incoming tail
        // retires source ownership before the state can be restored.
        let restore_state = unsafe { interrupt_mask.into_restore_state() };
        switch.activate(restore_state);
    }
}

pub fn initialize() -> Result<Capabilities, Error> {
    let cpu = current_cpu()?;
    if SCHEDULER.with(|slot| slot.is_some()) {
        return Err(Error::AlreadyInitialized);
    }
    let mut scheduler = Scheduler::new(cpu)?;
    let mut scratch = Some(queue::CpuRunQueue::prepare_scratch()?);
    let preemption = super::preempt::prepare_cpu(cpu)?;
    let capabilities = SCHEDULER.with(|slot| {
        if slot.is_some() {
            return Err(Error::AlreadyInitialized);
        }
        scheduler.activate_boot_cpu(cpu, &mut scratch)?;
        *slot = Some(scheduler);
        Ok(Capabilities {
            bootstrap_thread: ThreadId::BOOTSTRAP,
        })
    })?;
    #[cfg(feature = "kernel-trace")]
    crate::diagnostics::trace::publish_actor(cpu, ThreadId::BOOTSTRAP.get());
    preemption.commit();
    Ok(capabilities)
}

/// Permanent scheduler service population used by lifecycle self-tests.
#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
pub(crate) const fn permanent_worker_count_for_test() -> usize {
    1
}

pub fn current_thread_id() -> Result<ThreadId, Error> {
    // SAFETY: Close CPU identification through its local scheduler lookup;
    // an IRQ-tail migration must not substitute another Thread's identity.
    let _mask = unsafe { TransitionMask::acquire() };
    let cpu = current_cpu()?;
    state::local_current_thread(cpu)
}

/// Retains the current user Thread for cancellation checks across scheduling.
/// Kernel and bootstrap Threads have no user object. No execution pointer or
/// CPU-affine borrow escapes the local scheduler lock.
pub(crate) fn current_user_thread() -> Result<Option<crate::process::UserThread>, Error> {
    // SAFETY: The returned object is owned, but identifying which local slot
    // to retain must complete on the same CPU as the scheduler observation.
    let _mask = unsafe { TransitionMask::acquire() };
    let cpu = current_cpu()?;
    state::local_current_user_thread(cpu)
}

/// Resolves a diagnostic ID for privileged trace export, without granting authority.
#[cfg(feature = "kernel-trace")]
pub(crate) fn trace_thread_name(id: u64) -> Result<ThreadNameSnapshot, Error> {
    thread_name(ThreadId::from_diagnostic(id))
}

/// Owned, authority-free join between recorder and object-inspector identities.
#[cfg(feature = "kernel-trace")]
pub(crate) fn trace_thread_details(
    id: u64,
) -> Result<(ThreadNameSnapshot, crate::object::Koid), Error> {
    read_scheduler(|scheduler| {
        scheduler.with_thread(ThreadId::from_diagnostic(id), |thread| {
            (thread.name_snapshot(), thread.object_snapshot().object.koid)
        })
    })
}

/// Returns an owned name snapshot for the identified scheduler Thread.
pub fn thread_name(id: ThreadId) -> Result<ThreadNameSnapshot, Error> {
    read_scheduler(|scheduler| scheduler.with_thread(id, Thread::name_snapshot))
}

/// Replaces a live Thread's diagnostic label under its independent name lock.
/// Native callers are restricted to their own scheduler ID by the entry service.
pub(crate) fn set_thread_name(id: ThreadId, name: &str) -> Result<(), Error> {
    read_scheduler(|scheduler| scheduler.with_thread(id, |thread| thread.set_name(name)))?
        .map_err(Error::Thread)
}

/// Returns authority-free canonical object identity for a scheduler Thread.
#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
pub(crate) fn thread_object_snapshot(
    id: ThreadId,
) -> Result<crate::task::ThreadObjectSnapshot, Error> {
    read_scheduler(|scheduler| scheduler.thread_object_snapshot(id))
}

/// Captures one pointer-free page of scheduler-to-object relationships.
pub(crate) fn scan_thread_objects(
    cursor: crate::task::ThreadObjectScanCursor,
    output: &mut crate::task::ThreadObjectSnapshotPage,
) -> Result<(), Error> {
    read_scheduler(|scheduler| {
        scheduler.scan_thread_objects(cursor, output);
        Ok(())
    })
}

pub fn statistics() -> Result<Statistics, Error> {
    let mut statistics = read_scheduler(|scheduler| Ok(scheduler.statistics()))?;
    // Population must be observed first. A concurrent detach either remains
    // represented in that snapshot or increments the counter under the same
    // scheduler critical section before this acquire observation.
    statistics.retirements_in_progress = RETIREMENTS_IN_PROGRESS.load(Ordering::Acquire);
    Ok(statistics)
}

pub fn thread_stack_statistics(
    id: ThreadId,
) -> Result<Option<crate::mm::stack::StackStatistics>, Error> {
    SCHEDULER.with(|slot| {
        slot.as_mut()
            .ok_or(Error::NotInitialized)?
            .stack_statistics(id)
    })
}

/// Captures current-task metadata without waiting on a potentially held lock.
pub(crate) fn crash_snapshot(cpu: usize) -> Option<CrashTaskSnapshot> {
    let cpu = CpuIndex::new(cpu)?;
    state::try_cpu_snapshot(cpu)
}

pub fn register_secondary_cpu(cpu: CpuIndex) -> Result<SecondaryStack, Error> {
    let scratch = queue::CpuRunQueue::prepare_scratch()?;
    let preemption = super::preempt::prepare_cpu(cpu)?;
    let reservation = reserve_thread(|scheduler| scheduler.reserve_secondary(cpu))?;
    let thread = match prepare_boxed_thread(Thread::secondary_bootstrap(reservation.id(), cpu)) {
        Ok(thread) => thread,
        Err(error) => {
            abandon_reservation(reservation)?;
            return Err(error);
        }
    };
    let stack = publish_secondary(reservation, thread, scratch)?;
    preemption.commit();
    Ok(stack)
}

pub fn kthread_create(
    name: &str,
    entry: KernelThreadEntry,
    argument: usize,
) -> Result<ThreadId, Error> {
    match kthread_create_with_policy_and_affinity(
        name,
        entry,
        argument,
        SchedulingPolicy::fair(),
        CpuMask::ALL,
    ) {
        Ok(id) => {
            pr_debug!("HypeR: created kernel thread {} with id {}", name, id.get());
            Ok(id)
        }
        Err(error) => {
            pr_debug!(
                "HypeR: failed to create kernel thread {} with error {:?}",
                name,
                error
            );
            Err(error)
        }
    }
}

/// Creates a dormant kernel thread constrained to `affinity`.
///
/// Initial Fair placement selects the least runnable admitted CPU, retaining
/// creator locality on a tie. Explicit migration and affinity updates retain
/// the supplied mask.
pub fn kthread_create_with_affinity(
    name: &str,
    entry: KernelThreadEntry,
    argument: usize,
    affinity: CpuMask,
) -> Result<ThreadId, Error> {
    kthread_create_with_policy_and_affinity(
        name,
        entry,
        argument,
        SchedulingPolicy::fair(),
        affinity,
    )
}

/// Creates a dormant Fair worker permanently owned by one admitted CPU.
///
/// Its name is `name/CPU`. Unlike a movable thread with singleton affinity,
/// this worker rejects later affinity changes and explicit migration too.
pub(crate) fn kthread_create_per_cpu(
    name: &str,
    cpu: CpuIndex,
    entry: KernelThreadEntry,
    argument: usize,
) -> Result<ThreadId, Error> {
    let name = ThreadNameSnapshot::per_cpu(name, cpu)?;
    let policy = SchedulingPolicy::fair();
    let reservation = reserve_thread(|scheduler| {
        scheduler.reserve_kernel_thread(cpu, CpuMask::single(cpu), policy)
    })?;
    let prepared = Thread::kernel(
        reservation.id(),
        ThreadPlacement::pinned(cpu),
        name.as_str(),
        entry,
        argument,
    );
    finish_kernel_thread(reservation, prepared, policy)
}

/// Validates that an affinity mask currently contains an admitted CPU.
///
/// This is a point-in-time admission check. CPU hotplug may change placement
/// availability later, so final Thread creation repeats the same validation.
pub(crate) fn validate_affinity(affinity: CpuMask) -> Result<(), Error> {
    let preferred_cpu = current_cpu()?;
    read_scheduler(|scheduler| scheduler.validate_affinity(preferred_cpu, affinity))
}

/// Creates a dormant real-time FIFO kernel thread.
pub fn kthread_create_fifo(
    name: &str,
    entry: KernelThreadEntry,
    argument: usize,
    priority: ThreadPriority,
) -> Result<ThreadId, Error> {
    kthread_create_fifo_with_affinity(name, entry, argument, priority, CpuMask::ALL)
}

/// Creates a dormant real-time FIFO kernel thread with explicit affinity.
pub fn kthread_create_fifo_with_affinity(
    name: &str,
    entry: KernelThreadEntry,
    argument: usize,
    priority: ThreadPriority,
    affinity: CpuMask,
) -> Result<ThreadId, Error> {
    kthread_create_with_policy_and_affinity(
        name,
        entry,
        argument,
        SchedulingPolicy::fifo(priority),
        affinity,
    )
}

fn kthread_create_with_policy_and_affinity(
    name: &str,
    entry: KernelThreadEntry,
    argument: usize,
    policy: SchedulingPolicy,
    affinity: CpuMask,
) -> Result<ThreadId, Error> {
    let cpu = current_cpu()?;
    let reservation =
        reserve_thread(|scheduler| scheduler.reserve_kernel_thread(cpu, affinity, policy))?;
    let prepared = ThreadPlacement::movable_with_affinity(reservation.cpu(), affinity)
        .ok_or(super::thread::Error::InvalidPlacement)
        .and_then(|placement| Thread::kernel(reservation.id(), placement, name, entry, argument));
    finish_kernel_thread(reservation, prepared, policy)
}

fn finish_kernel_thread(
    reservation: ThreadReservation,
    prepared: Result<Thread, super::thread::Error>,
    policy: SchedulingPolicy,
) -> Result<ThreadId, Error> {
    let id = reservation.id();
    let mut thread = match prepare_boxed_thread(prepared) {
        Ok(thread) => thread,
        Err(error) => {
            abandon_reservation(reservation)?;
            return Err(error);
        }
    };
    if !thread.set_scheduling_policy(policy) {
        abandon_reservation(reservation)?;
        drop(thread);
        return Err(Error::InvalidThreadState);
    }
    if policy == SchedulingPolicy::Fair {
        thread.with_coordinator_schedule_mut(|schedule| {
            schedule.install_parameters(configuration::deferred_parameters())
        });
    }
    publish_thread(reservation, thread)?;
    Ok(id)
}

pub(crate) fn thread_placement(id: ThreadId) -> Result<(CpuIndex, CpuMask), Error> {
    SCHEDULER.with(|slot| {
        slot.as_mut()
            .ok_or(Error::NotInitialized)?
            .thread_placement(id)
    })
}

/// Validates image-wide constraints without changing an entity or its budget.
pub(crate) fn validate_scheduling_parameters(
    parameters: SchedulingParameters,
) -> Result<(), Error> {
    if !configuration::arbitration().admits(
        parameters,
        default_parameters(SchedulingPolicy::Fair).fair_backend(),
    ) {
        return Err(Error::InvalidSchedulingParameters);
    }
    Ok(())
}

pub(crate) fn scheduling_snapshot(id: ThreadId) -> Result<SchedulingParameters, Error> {
    SCHEDULER.with(|slot| {
        slot.as_mut()
            .ok_or(Error::NotInitialized)?
            .scheduling_snapshot(id)
    })
}

/// Capability validation and ready-queue reindexing form one transaction.
pub(crate) fn configure_scheduling(
    id: ThreadId,
    parameters: SchedulingParameters,
    authority: super::scheduling_authority::SchedulingAuthority,
    limits: super::scheduling_authority::SchedulingLimits,
) -> Result<(), Error> {
    validate_scheduling_parameters(parameters)?;
    let target = SCHEDULER.with(|slot| {
        slot.as_mut()
            .ok_or(Error::NotInitialized)?
            .configure_scheduling(id, parameters, authority, limits)
    })?;
    if let Some(cpu) = target
        && let Err(error) = request_reschedule(cpu, RescheduleSource::Policy)
    {
        // The parameters and ready-queue placement are already committed.
        // Returning a normal rejection here would misrepresent that state.
        scheduler_invariant("committed scheduling notification", error);
    }
    Ok(())
}

/// Observes assignment and the accepted, not-yet-committed migration target.
/// Assignment is not a claim that the Thread is currently executing.
pub(crate) fn thread_migration_state(id: ThreadId) -> Result<(CpuIndex, Option<CpuIndex>), Error> {
    SCHEDULER.with(|slot| {
        slot.as_mut()
            .ok_or(Error::NotInitialized)?
            .thread_migration_state(id)
    })
}

#[cfg(all(
    target_arch = "aarch64",
    feature = "kernel-test",
    feature = "kernel-self-test"
))]
pub(crate) fn thread_registry_slot_for_test(id: ThreadId) -> Option<usize> {
    id.scheduler_slot()
}

#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
pub(crate) fn registry_slot_count() -> Result<usize, Error> {
    SCHEDULER.with(|slot| {
        slot.as_ref()
            .ok_or(Error::NotInitialized)
            .map(Scheduler::registry_slot_count)
    })
}

#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
pub(crate) fn discard_dormant_kernel_thread(id: ThreadId) -> Result<(), Error> {
    let thread = SCHEDULER.with(|slot| {
        slot.as_mut()
            .ok_or(Error::NotInitialized)?
            .take_dormant_kernel_thread(id)
    })?;
    drop(thread);
    Ok(())
}

pub(crate) fn vcpu_create(
    name: &str,
    topology_index: u32,
    execution: ExternalThreadExecution,
    resources: ThreadResourceOwnership,
    entry: KernelThreadEntry,
) -> Result<DormantVcpuThread, Error> {
    let cpu = current_cpu()?;
    let reservation =
        reserve_thread(|scheduler| scheduler.reserve_vcpu_thread(cpu, topology_index))?;
    let id = reservation.id();
    let thread = match prepare_boxed_thread(Thread::vcpu(
        id,
        reservation.cpu(),
        name,
        execution,
        resources,
        entry,
    )) {
        Ok(thread) => thread,
        Err(error) => {
            abandon_reservation(reservation)?;
            return Err(error);
        }
    };
    publish_thread(reservation, thread)?;
    Ok(DormantVcpuThread {
        thread: id,
        rollback: true,
    })
}

pub(crate) fn prepare_user_thread_with_scheduling(
    name: &str,
    object: crate::process::UserThread,
    execution: alloc::boxed::Box<core::cell::UnsafeCell<crate::process::UserExecution>>,
    entry: KernelThreadEntry,
    affinity: CpuMask,
    parameters: SchedulingParameters,
) -> Result<DormantUserThread, Error> {
    validate_scheduling_parameters(parameters)?;
    let cpu = current_cpu()?;
    let reservation = reserve_thread(|scheduler| {
        scheduler.reserve_kernel_thread(cpu, affinity, parameters.policy())
    })?;
    let id = reservation.id();
    let mut thread = match prepare_boxed_thread(Thread::user(
        id,
        reservation.cpu(),
        affinity,
        name,
        object,
        execution,
        entry,
    )) {
        Ok(thread) => thread,
        Err(error) => {
            abandon_reservation(reservation)?;
            return Err(error);
        }
    };
    thread.with_coordinator_schedule_mut(|schedule| schedule.install_parameters(parameters));
    publish_thread(reservation, thread)?;
    Ok(DormantUserThread {
        thread: id,
        rollback: true,
    })
}

pub(crate) fn request_user_thread_stop(
    id: ThreadId,
    reason: crate::process::TerminalReason,
) -> Result<(), Error> {
    let (target, retirement_published) = SCHEDULER.with(|slot| {
        let scheduler = slot.as_mut().ok_or(Error::NotInitialized)?;
        let target = scheduler.request_user_stop(id, reason)?;
        let retirement_published = scheduler.queue_terminated_retirement(id)?;
        Ok::<_, Error>((target, retirement_published))
    })?;
    if retirement_published {
        crate::reaper::request();
    }
    if let Some(cpu) = target {
        request_reschedule(cpu, RescheduleSource::Stop)?;
    }
    Ok(())
}

fn prepare_boxed_thread(
    thread: Result<Thread, super::thread::Error>,
) -> Result<Box<Thread>, Error> {
    hyper::mm::try_box(thread?).map_err(|_| Error::Allocation)
}

fn reserve_thread(
    reserve: impl Fn(&mut Scheduler) -> Result<ThreadReservation, Error>,
) -> Result<ThreadReservation, Error> {
    let mut growth = None;
    loop {
        let result = SCHEDULER.with(|slot| {
            let scheduler = slot.as_mut().ok_or(Error::NotInitialized)?;
            if let Some(growth) = growth.as_mut() {
                scheduler.install_thread_table_growth(growth);
            }
            reserve(scheduler)
        });
        // Unused competing allocations are always destroyed outside the lock.
        drop(growth.take());
        match result {
            Err(Error::ThreadTableGrowth) => {
                if let Some(segment) =
                    read_scheduler(|scheduler| Ok(scheduler.thread_table_growth()))?
                {
                    growth = Some(registry::TableGrowth::prepare(segment)?);
                }
            }
            result => return result,
        }
    }
}

fn publish_thread(mut reservation: ThreadReservation, thread: Box<Thread>) -> Result<(), Error> {
    let (result, retired) = SCHEDULER.with(|slot| match slot.as_mut() {
        Some(scheduler) => {
            let result = match scheduler.publish_thread(&reservation, thread) {
                Ok(()) => Ok(()),
                Err((error, thread)) => match scheduler.abandon_reservation(&reservation) {
                    Ok(()) => Err((error, thread)),
                    Err(_) => hyper::debug::invariant_failure(
                        "task::scheduler::mod::publish_thread invariant",
                    ),
                },
            };
            (result, true)
        }
        None => (Err((Error::NotInitialized, thread)), false),
    });
    if retired {
        reservation.disarm();
    }
    match result {
        Ok(()) => Ok(()),
        Err((error, thread)) => {
            drop(thread);
            Err(error)
        }
    }
}

fn publish_secondary(
    mut reservation: ThreadReservation,
    thread: Box<Thread>,
    scratch: queue::QueueScratch,
) -> Result<SecondaryStack, Error> {
    // Unpublished queue storage is released outside the coordinator on error.
    let mut scratch = Some(scratch);
    let (result, retired) = SCHEDULER.with(|slot| match slot.as_mut() {
        Some(scheduler) => {
            let result = match scheduler.publish_secondary(&reservation, thread, &mut scratch) {
                Ok(stack) => Ok(stack),
                Err((error, thread)) => match scheduler.abandon_reservation(&reservation) {
                    Ok(()) => Err((error, thread)),
                    Err(_) => hyper::debug::invariant_failure(
                        "task::scheduler::mod::publish_secondary invariant",
                    ),
                },
            };
            (result, true)
        }
        None => (Err((Error::NotInitialized, thread)), false),
    });
    if retired {
        reservation.disarm();
    }
    match result {
        Ok(stack) => Ok(stack),
        Err((error, thread)) => {
            drop(thread);
            Err(error)
        }
    }
}

fn abandon_reservation(mut reservation: ThreadReservation) -> Result<(), Error> {
    SCHEDULER.with(|slot| {
        slot.as_mut()
            .ok_or(Error::NotInitialized)?
            .abandon_reservation(&reservation)
    })?;
    reservation.disarm();
    Ok(())
}

/// Returns the pinned vCPU payload owned by the calling CPU's current Thread.
pub(crate) fn current_vcpu() -> Result<CurrentVcpu, Error> {
    state::local_current_vcpu(current_cpu()?)?.ok_or(Error::InvalidThreadState)
}

/// Returns the CPU which the scheduler currently proves is running `thread`.
///
/// This is a read-only location observation. VM policy owns guest-exit
/// prompting and must tolerate the Thread stopping or migrating after the
/// scheduler lock is released.
pub(crate) fn running_vcpu_cpu(thread: ThreadId) -> Result<Option<CpuIndex>, Error> {
    read_scheduler(|scheduler| scheduler.running_vcpu_cpu(thread))
}

/// Prompts the exact scheduler-owned vCPU after its endpoint accepted a stop.
///
/// This operation never tears down a suspended continuation. Dormant and
/// Ready Threads run their fixed entry path; a WFI waiter was already made
/// Ready by endpoint notification; Running/Migrating contexts receive only a
/// nonblocking prompt and unwind at a qualified architecture boundary.
pub(crate) fn request_vcpu_stop(thread: ThreadId) -> Result<(), Error> {
    let (target, retirement_published) = SCHEDULER.with(|slot| {
        let scheduler = slot.as_mut().ok_or(Error::NotInitialized)?;
        let target = scheduler.request_vcpu_stop(thread)?;
        let retirement_published = scheduler.queue_terminated_retirement(thread)?;
        Ok::<_, Error>((target, retirement_published))
    })?;
    if retirement_published {
        crate::reaper::request();
    }
    match target {
        state::VcpuStopTarget::Running(cpu) => {
            if !crate::hal::vm::request_guest_exit(cpu) {
                let target = SCHEDULER.with(|slot| {
                    slot.as_ref()
                        .ok_or(Error::NotInitialized)?
                        .running_vcpu_cpu(thread)
                })?;
                match target {
                    None => {}
                    Some(target) if target != cpu && crate::hal::vm::request_guest_exit(target) => {
                    }
                    Some(_) => scheduler_invariant(
                        "administrative vCPU guest-exit prompt",
                        Error::PreemptionUnavailable,
                    ),
                }
            }
        }
        state::VcpuStopTarget::Migrating(cpu) => request_reschedule(cpu, RescheduleSource::Stop)?,
        state::VcpuStopTarget::Runnable { cpu, ready } => {
            if let Some(ready) = ready {
                publish_ready_outcome(ready)?;
            }
            request_reschedule(cpu, RescheduleSource::Stop)?;
        }
        state::VcpuStopTarget::Terminated => {}
    }
    Ok(())
}

/// Returns the pinned native-user payload owned by the current Thread.
///
/// The short pin closes the identity lookup. The returned allocation belongs
/// to this Thread until its entry returns, including while its continuation
/// blocks or migrates; no CPU-local authority is transferred with the pointer.
pub(crate) fn current_user(_pin: &PreemptionGuard) -> Result<CurrentUser, Error> {
    state::local_current_user(current_cpu()?)
}

/// Returns the pinned vCPU payload when the current Thread owns one.
///
/// This does not borrow or publish the execution. The raw owner pointer is
/// consumed only by the masked architecture IRQ-tail continuation, where the
/// current Thread cannot change before the scheduler transaction begins.
pub(crate) fn current_vcpu_if_present() -> Result<Option<CurrentVcpu>, Error> {
    let cpu = current_cpu()?;
    state::local_current_vcpu(cpu)
}

/// Enqueues a dormant thread on its owning CPU's priority ready queue.
pub fn thread_ready(id: ThreadId) -> Result<bool, Error> {
    let outcome =
        SCHEDULER.with(|slot| slot.as_mut().ok_or(Error::NotInitialized)?.make_ready(id))?;
    publish_ready_outcome(outcome)?;
    Ok(outcome.changed)
}

/// Publishes native-user lifecycle and scheduler readiness as one transaction.
pub(crate) fn ready_user_thread(id: ThreadId) -> Result<bool, Error> {
    let outcome = SCHEDULER.with(|slot| -> Result<state::ReadyOutcome, Error> {
        let scheduler = slot.as_mut().ok_or(Error::NotInitialized)?;
        scheduler.make_user_ready(id)
    })?;
    // Readiness is committed: an ordinary error would let a spawn caller free
    // the user stack even though the target can already execute on another CPU.
    publish_committed_ready(outcome);
    Ok(outcome.changed)
}

/// Moves a movable Thread to one allowed registered CPU.
///
/// Dormant, Ready, and fully stopped Blocked Threads move synchronously. A
/// running or switch-in-flight Thread returns [`MigrationStatus::Pending`]
/// when deferred handoff is accepted; target publication occurs only after the
/// source context is saved and may finish before the caller observes the return.
/// Kernel, user, and vCPU Threads use the same saved-context handoff. User
/// address spaces and vCPU hardware are detached before the scheduling point
/// and reactivated on the destination CPU. Idle, bootstrap and per-CPU workers
/// remain pinned.
pub fn migrate_thread(id: ThreadId, target: CpuIndex) -> Result<MigrationStatus, Error> {
    let outcome = SCHEDULER.with(|slot| {
        slot.as_mut()
            .ok_or(Error::NotInitialized)?
            .migrate_thread(id, target)
    })?;
    Ok(publish_migration_outcome(outcome))
}

/// Replaces a movable Thread's CPU affinity and migrates it when required.
///
/// The affinity update and assignment change are one scheduler transaction.
/// If the current assignment remains allowed, the Thread keeps its run-queue
/// position. Otherwise the completion semantics match [`migrate_thread`].
pub fn set_thread_affinity(id: ThreadId, affinity: CpuMask) -> Result<MigrationStatus, Error> {
    let outcome = SCHEDULER.with(|slot| {
        slot.as_mut()
            .ok_or(Error::NotInitialized)?
            .set_thread_affinity(id, affinity)
    })?;
    Ok(publish_migration_outcome(outcome))
}

/// Assigns or updates a thread's real-time FIFO policy.
///
/// Calling this for a Fair thread is an explicit class transition into RT.
pub fn set_thread_fifo_policy(id: ThreadId, priority: ThreadPriority) -> Result<(), Error> {
    let target = update_thread_fifo_policy(id, priority)?;
    if let Some(cpu) = target {
        request_reschedule(cpu, RescheduleSource::Policy)?;
    }
    Ok(())
}

fn update_thread_fifo_policy(
    id: ThreadId,
    priority: ThreadPriority,
) -> Result<Option<CpuIndex>, Error> {
    SCHEDULER.with(|slot| {
        slot.as_mut()
            .ok_or(Error::NotInitialized)?
            .set_fifo_policy(id, priority)
    })
}

/// Applies FIFO policy and reports whether the transition prompted arbitration.
#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
pub(crate) fn set_thread_fifo_policy_for_test(
    id: ThreadId,
    priority: ThreadPriority,
) -> Result<bool, Error> {
    let target = update_thread_fifo_policy(id, priority)?;
    if let Some(cpu) = target {
        request_reschedule(cpu, RescheduleSource::Policy)?;
    }
    Ok(target.is_some())
}

/// Assigns a thread to the ordinary Fair scheduling class.
///
/// Ready threads move atomically between algorithm queues. Every changed ready
/// or running policy prompts arbitration; the caller never switches inside this
/// operation or decides eligibility using a static ordering of classes.
pub fn set_thread_fair_policy(id: ThreadId) -> Result<(), Error> {
    let target = SCHEDULER.with(|slot| {
        slot.as_mut()
            .ok_or(Error::NotInitialized)?
            .set_fair_policy(id)
    })?;
    if let Some(cpu) = target {
        request_reschedule(cpu, RescheduleSource::Policy)?;
    }
    Ok(())
}

/// A one-shot deadline only requests a safe scheduling point. A cancelled or
/// replaced deadline cannot preempt a later generation of CPU scheduling.
pub(crate) fn deadline_expired(generation: u64) -> Result<(), Error> {
    let cpu = current_cpu()?;
    if state::deadline_is_current(cpu, generation) {
        request_reschedule(cpu, RescheduleSource::Deadline)?;
    }
    Ok(())
}

#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
pub(crate) fn expire_current_turn_for_test() -> Result<(), Error> {
    let cpu = current_cpu()?;
    state::expire_current_turn(cpu)?;
    request_reschedule(cpu, RescheduleSource::Deadline)
}

pub fn yield_now() -> Result<(), Error> {
    ensure_sleepable()?;
    if let Some(pair) = prepare_schedule()? {
        pair.activate();
    }
    Ok(())
}

/// Reconsiders the current thread at an explicit safe point.
///
/// Unlike `yield_now`, this rotates Fair peers only after slice expiry and
/// never rotates equal-priority RT FIFO peers.
pub fn cond_resched() -> Result<bool, Error> {
    ensure_sleepable()?;
    cond_resched_inner(SchedulePoint::SafePoint)
}

/// Reconsiders scheduling from an outermost architecture IRQ continuation.
///
/// IRQ accounting and controller completion must already be complete. The
/// opaque capability is minted only by a selected architecture return path
/// which provides private-stack continuation and interrupt-state transfer.
// Secondary targets cannot mint `IrqTailCapability`; keep the common policy
// entry compiled so target selection does not leak into scheduler code.
#[allow(dead_code)]
pub(crate) fn cond_resched_from_irq_tail(
    tail: crate::hal::exception::IrqTailCapability,
) -> Result<bool, Error> {
    if crate::hal::irq::local_enabled() {
        return Err(Error::IrqTailRequiresInterruptsMasked);
    }
    let cpu = current_cpu()?;
    if !super::preempt::can_reschedule(cpu)? {
        return Ok(false);
    }
    // This proof is CPU-affine. Consume it before the context switch can move
    // the interrupted continuation; restored IRQ-mask state, not a live Rust
    // value, governs execution after the switch.
    tail.consume();
    cond_resched_inner(SchedulePoint::IrqTail)
}

fn cond_resched_inner(point: SchedulePoint) -> Result<bool, Error> {
    // SAFETY: This continuation transfers the exact prior interrupt state into
    // its saved ThreadContext at the final machine-switch boundary. Acquire
    // before balancing so its continuation prompt cannot reenter scheduling
    // between the placement pass and this CPU's selection.
    let interrupt_mask = unsafe { TransitionMask::acquire() };
    balance::service();
    let cpu = current_cpu()?;
    service_deadline_publication()?;
    if !super::preempt::pending(cpu)? || !super::preempt::can_reschedule(cpu)? {
        return Ok(false);
    }
    let pair = match state::prepare_local_preemption(cpu, point)? {
        state::LocalScheduleAttempt::Complete(pair) => pair,
        state::LocalScheduleAttempt::NeedsCoordinator => SCHEDULER.with(|slot| {
            let scheduler = slot.as_mut().ok_or(Error::NotInitialized)?;
            scheduler.prepare_preemption(cpu, point)
        })?,
    };
    service_deadline_publication()?;
    let Some(pair) = pair else {
        return Ok(false);
    };
    PreparedTransition {
        switch: pair,
        interrupt_mask,
    }
    .activate();
    Ok(true)
}

/// Capability proving that the current thread cannot pass a preemption point.
///
/// The guard is CPU-local. It may protect a bounded CPU-local borrow but must
/// not cross a blocking operation or guest entry.
#[must_use = "dropping the guard restores preemption without scheduling"]
pub struct PreemptionGuard(super::preempt::PreemptionGuard);

// SAFETY: The underlying per-CPU disable-depth owner prevents every scheduler
// preemption point from migrating this context until the guard is released.
unsafe impl hyper::cpu::PinnedExecution for PreemptionGuard {}

/// Prevents asynchronous scheduling while CPU-local state is borrowed.
pub fn preempt_disable() -> Result<PreemptionGuard, Error> {
    super::preempt::disable()
        .map(PreemptionGuard)
        .map_err(Into::into)
}

/// Releases a preemption guard without introducing a scheduling point.
///
/// This is reserved for nested protocols that must restore preemption
/// accounting before returning to an outer owner. Deferred work remains
/// pending until that owner reaches its own safe scheduling point.
pub(crate) fn preempt_enable_without_reschedule(guard: PreemptionGuard) -> Result<(), Error> {
    guard.0.release().map(|_| ()).map_err(Into::into)
}

/// Releases a preemption guard and immediately observes deferred requests.
pub fn preempt_enable_and_reschedule(guard: PreemptionGuard) -> Result<bool, Error> {
    if guard.0.release()? {
        cond_resched()
    } else {
        Ok(false)
    }
}

pub(crate) fn install_current_idle() -> Result<(usize, usize), Error> {
    let cpu = current_cpu()?;
    SCHEDULER.with(|slot| {
        slot.as_mut()
            .ok_or(Error::NotInitialized)?
            .install_current_as_idle(cpu)
    })
}

extern "C" fn enter_clean_idle(_argument: usize) -> ! {
    crate::hal::irq::enable_local();
    run_idle_loop()
}

extern "C" fn idle_thread_entry(argument: usize) {
    enter_clean_idle(argument)
}

pub(crate) fn run_idle_loop() -> ! {
    run_idle_loop_inner(false)
}

/// Enters the secondary idle loop and publishes readiness after the first
/// protected queue observation.
pub(crate) fn run_secondary_idle_loop() -> ! {
    run_idle_loop_inner(true)
}

fn run_idle_loop_inner(mut publish_secondary_online: bool) -> ! {
    loop {
        if let Err(error) = idle_wait_or_schedule(&mut publish_secondary_online) {
            crate::diagnostics::crash::fatal(format_args!(
                "HypeR: idle scheduling failed: {error:?}"
            ));
        }
    }
}

/// Closes the idle queue-check-to-sleep window under one interrupt mask.
fn idle_wait_or_schedule(publish_secondary_online: &mut bool) -> Result<(), Error> {
    // SAFETY: The guard remains on this CPU. It is either consumed directly
    // into an outgoing context or dropped after the architecture returns from
    // its masked idle wait. Balance prompts and CPU observation share this
    // same outer mask with ready-queue selection.
    let interrupt_mask = unsafe { TransitionMask::acquire() };
    balance::service();
    let cpu = current_cpu()?;
    let switch = match state::prepare_local_yield(cpu)? {
        state::LocalScheduleAttempt::Complete(switch) => switch,
        state::LocalScheduleAttempt::NeedsCoordinator => SCHEDULER.with(|slot| {
            let scheduler = slot.as_mut().ok_or(Error::NotInitialized)?;
            scheduler.prepare_yield(cpu)
        })?,
    };
    if *publish_secondary_online {
        // The first ready-queue observation completed under this CPU's local
        // scheduler lock. Publish only after that point, while local IRQs
        // remain masked, so boot-side enqueue can at worst leave a pending
        // wakeup for the wait instruction below.
        crate::cpu::publish_current_online_from_idle_observation();
        *publish_secondary_online = false;
    }
    service_deadline_publication()?;
    if let Some(switch) = switch {
        PreparedTransition {
            switch,
            interrupt_mask,
        }
        .activate();
    } else {
        crate::hal::cpu::wait_for_interrupt_masked();
        drop(interrupt_mask);
    }
    Ok(())
}

pub(crate) fn prepare_park(wait_queue: &WaitQueue) -> Result<ParkToken, Error> {
    ensure_sleepable()?;
    // SAFETY: The park token retains the outer mask until its exact saved state
    // is consumed into the outgoing ThreadContext at the machine-switch boundary.
    let interrupt_mask = unsafe { TransitionMask::acquire() };
    let registration = begin_wait(WaitMobility::Migratable)?;
    match prepare_registered_park_locked(wait_queue, registration)? {
        PrepareWait::Park(commit) => Ok(retain_park_mask(commit, interrupt_mask)),
        PrepareWait::Completed(_) => Err(Error::InvalidWaitRegistration),
    }
}

/// Arms a generation-qualified wait owned by the current Thread.
pub(crate) fn begin_wait(mobility: WaitMobility) -> Result<WaitRegistration, Error> {
    let ticket = read_scheduler(|scheduler| scheduler.arm_wait_shared(current_cpu()?, mobility))?;
    Ok(WaitRegistration {
        ticket,
        active: true,
        not_send_or_sync: PhantomData,
    })
}

/// Consumes an unqueued registration under its condition-object lock.
///
/// `None` means no resolver won and the caller may consume the protected
/// condition. `Some` means timeout/cancellation already won and condition
/// state must remain untouched.
pub(crate) fn finish_wait(
    mut registration: WaitRegistration,
) -> Result<Option<WaitOutcome>, Error> {
    let result = read_scheduler(|scheduler| {
        scheduler.finish_wait_shared(current_cpu()?, registration.ticket, false)
    });
    if result.is_ok() {
        registration.disarm();
    }
    result
}

/// Atomically queues an armed registration or consumes an earlier resolution.
///
/// The caller holds the condition object's IRQ-masking lock, closing the
/// condition-check-to-park window. Queue membership and the embedded wait
/// record are published under the owner CPU lock and the queue lock.
pub(crate) fn prepare_registered_park_locked(
    wait_queue: &WaitQueue,
    mut registration: WaitRegistration,
) -> Result<PrepareWait, Error> {
    let cpu = current_cpu()?;
    let result = read_scheduler(|scheduler| {
        match scheduler.park_shared(cpu, wait_queue, registration.ticket)? {
            state::PreparedWait::Park { switch, ticket } => {
                Ok(PrepareWait::Park(ParkCommit { switch, ticket }))
            }
            state::PreparedWait::Completed(outcome) => Ok(PrepareWait::Completed(outcome)),
        }
    });
    if matches!(&result, Ok(_) | Err(Error::CurrentThreadMissing)) {
        // CurrentThreadMissing is the one recoverable prepare failure: state
        // rolled queue membership and WaitRecord back to Idle before return.
        registration.disarm();
    }
    result
}

/// Closes the registration-to-park boundary for a wait without an enclosing
/// condition-object lock.
pub(crate) fn prepare_registered_park(
    wait_queue: &WaitQueue,
    registration: WaitRegistration,
) -> Result<PreparedWait, Error> {
    // SAFETY: either the committed park consumes this exact mask state, or the
    // already-completed path drops it before returning to the caller.
    let interrupt_mask = unsafe { TransitionMask::acquire() };
    Ok(prepare_registered_park_locked(wait_queue, registration)?.retain_mask(interrupt_mask))
}

/// Binds a synchronization lock's retained interrupt mask to a committed park.
pub(crate) fn retain_park_mask(commit: ParkCommit, interrupt_mask: TransitionMask) -> ParkToken {
    ParkToken {
        transition: PreparedTransition {
            switch: commit.switch,
            interrupt_mask,
        },
        ticket: commit.ticket,
    }
}

pub(crate) fn complete_park(token: ParkToken) -> WaitOutcome {
    token.transition.activate();
    let result = read_scheduler(|scheduler| {
        scheduler.finish_wait_shared(current_cpu()?, token.ticket, true)
    })
    .and_then(|outcome| outcome.ok_or(Error::InvalidWaitRegistration));
    match result {
        Ok(outcome) => outcome,
        Err(error) => scheduler_invariant("committed wait completion", error),
    }
}

pub(crate) fn wake_one(wait_queue: &WaitQueue) -> Result<Option<ThreadId>, Error> {
    wake_one_with(wait_queue, |_| {})
}

pub(crate) fn wake_one_with(
    wait_queue: &WaitQueue,
    before_ready: impl FnOnce(ThreadId),
) -> Result<Option<ThreadId>, Error> {
    let awakened =
        read_scheduler(|scheduler| scheduler.notify_one_shared(wait_queue, before_ready))?;
    if let Some((_, outcome)) = awakened {
        publish_committed_ready(outcome);
    }
    Ok(awakened.map(|(id, _)| id))
}

pub(crate) fn wake_all(wait_queue: &WaitQueue) -> Result<usize, Error> {
    let (count, targets) = read_scheduler(|scheduler| {
        let mut count = 0usize;
        let mut targets = [false; hyper::cpu::MAX_CPUS];
        // Bound the batch by its initial population. New/requeued waiters
        // cannot keep an IRQ-masked wake-all operation alive indefinitely.
        let limit = scheduler.wait_queue_snapshot(wait_queue)?.len;
        for _ in 0..limit {
            let Some((_, outcome)) = scheduler.notify_one_shared(wait_queue, |_| {})? else {
                break;
            };
            if outcome.changed {
                targets[outcome.target_cpu.get()] = true;
            }
            count += 1;
        }
        Ok::<_, Error>((count, targets))
    })?;
    for (index, requested) in targets.into_iter().enumerate() {
        if requested {
            let Some(cpu) = CpuIndex::new(index) else {
                scheduler_invariant("wait-all CPU publication", Error::InvalidCpuIndex);
            };
            if let Err(error) = request_reschedule(cpu, RescheduleSource::Wakeup) {
                scheduler_invariant("wait-all ready publication", error);
            }
        }
    }
    if count != 0 {
        crate::hal::cpu::send_event();
    }
    Ok(count)
}

/// Exercises the public wake paths with inconsistent queue metadata, without
/// linking actual waiters or exposing a corruption hook to production callers.
#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
pub(crate) fn test_wait_queue_topology() -> Result<(), Error> {
    use super::wait::ThreadQueue;

    let queue = WaitQueue::new();
    if wake_one(&queue)?.is_some() || wake_all(&queue)? != 0 {
        return Err(Error::InvalidThreadState);
    }
    for head in [None, Some(ThreadId::BOOTSTRAP)] {
        for tail in [None, Some(ThreadId::BOOTSTRAP)] {
            for len in [0, 1] {
                // Valid empty and populated shapes are covered by normal
                // wait/wake tests; the dummy identity is never linked here.
                if (head.is_none() && tail.is_none() && len == 0)
                    || (head.is_some() && tail.is_some() && len != 0)
                {
                    continue;
                }
                let damaged = ThreadQueue { head, tail, len };
                read_scheduler(|scheduler| {
                    scheduler.with_wait_queue(&queue, |state| *state = damaged);
                    Ok(())
                })?;
                let mut callback_ran = false;
                let one = wake_one_with(&queue, |_| callback_ran = true);
                let all = wake_all(&queue);
                let unchanged = read_scheduler(|scheduler| {
                    Ok(scheduler.with_wait_queue(&queue, |state| {
                        let unchanged = *state == damaged;
                        *state = ThreadQueue::new();
                        unchanged
                    }))
                })?;
                if one != Err(Error::QueueCorrupted)
                    || all != Err(Error::QueueCorrupted)
                    || callback_ran
                    || !unchanged
                {
                    return Err(Error::InvalidThreadState);
                }
            }
        }
    }
    Ok(())
}

/// Attempts exact-ticket timeout or cancellation arbitration.
///
/// Stale tickets and already-completed registrations are clean losers. A
/// queued winner is made runnable under its CPU lock; reschedule/event
/// publication happens only after releasing scheduler protection.
pub(crate) fn resolve_wait(ticket: WaitTicket, outcome: WaitOutcome) -> Result<ResolveWait, Error> {
    if outcome == WaitOutcome::Notified {
        return Err(Error::InvalidWaitRegistration);
    }
    let resolved = read_scheduler(|scheduler| {
        scheduler.resolve_wait_shared(ticket, outcome, state::WakePreemption::Policy, || {})
    })?;
    if let Some(ready) = resolved.ready {
        publish_committed_ready(ready);
    }
    Ok(ResolveWait {
        won: resolved.won,
        made_ready: resolved.ready.is_some(),
    })
}

/// Notifies one exact wait generation and commits caller-owned result state.
///
/// `on_commit` executes synchronously under the owner CPU lock only when this
/// notification wins over timeout and cancellation. It runs before a queued
/// Thread becomes Ready, so an object may publish a typed observation without
/// storing that payload in scheduler policy. The callback must be bounded,
/// infallible, allocation-free, and must not re-enter the scheduler.
pub(crate) fn notify_registered_with(
    ticket: WaitTicket,
    on_commit: impl FnOnce(),
) -> Result<ResolveWait, Error> {
    let resolved = read_scheduler(|scheduler| {
        scheduler.resolve_wait_shared(
            ticket,
            WaitOutcome::Notified,
            state::WakePreemption::Policy,
            on_commit,
        )
    })?;
    if let Some(ready) = resolved.ready {
        publish_committed_ready(ready);
    }
    Ok(ResolveWait {
        won: resolved.won,
        made_ready: resolved.ready.is_some(),
    })
}

/// Notifies a latency-sensitive waiter and establishes an equal-Fair boundary.
///
/// This is intended for deadline and interrupt delivery. It preserves FIFO
/// class ordering while ensuring an ordinary Fair service thread cannot retain
/// the CPU beyond the IRQ tail after an equally Fair execution endpoint wakes.
pub(crate) fn notify_registered_fair_boundary(ticket: WaitTicket) -> Result<ResolveWait, Error> {
    let resolved = read_scheduler(|scheduler| {
        scheduler.resolve_wait_shared(
            ticket,
            WaitOutcome::Notified,
            state::WakePreemption::FairBoundary,
            || {},
        )
    })?;
    if let Some(ready) = resolved.ready {
        publish_committed_ready(ready);
    }
    Ok(ResolveWait {
        won: resolved.won,
        made_ready: resolved.ready.is_some(),
    })
}

pub(crate) fn cancel_waiter(wait_queue: &WaitQueue, id: ThreadId) -> Result<bool, Error> {
    let resolved = read_scheduler(|scheduler| scheduler.cancel_waiter_shared(wait_queue, id))?;
    if let Some(ready) = resolved.ready {
        publish_committed_ready(ready);
    }
    Ok(resolved.won)
}

pub(crate) fn waiter_count(wait_queue: &WaitQueue) -> Result<usize, Error> {
    read_scheduler(|scheduler| Ok(scheduler.with_wait_queue(wait_queue, |queue| queue.len)))
}

pub(crate) fn ensure_sleepable() -> Result<(), Error> {
    if !crate::hal::irq::local_enabled() {
        return Err(Error::CannotSleepWithInterruptsMasked);
    }
    // SAFETY: Pin this bounded CPU-local depth observation. The mask restores
    // the enabled state before the caller reaches any scheduling operation.
    let _mask = unsafe { TransitionMask::acquire() };
    let cpu = current_cpu()?;
    super::preempt::can_reschedule(cpu)?
        .then_some(())
        .ok_or(Error::CannotSleepWithPreemptionDisabled)
}

fn publish_ready_outcome(outcome: state::ReadyOutcome) -> Result<(), Error> {
    if outcome.changed {
        balance::request();
        request_reschedule(outcome.target_cpu, RescheduleSource::Wakeup)?;
    }
    Ok(())
}

fn publish_committed_ready(outcome: state::ReadyOutcome) {
    if let Err(error) = publish_ready_outcome(outcome) {
        scheduler_invariant("committed ready publication", error);
    }
}

fn publish_migration_outcome(outcome: state::MigrationOutcome) -> MigrationStatus {
    if let Some(ready) = outcome.target_ready {
        publish_committed_ready(ready);
    }
    if let Some(source) = outcome.source_reschedule
        && let Err(error) = request_reschedule(source, RescheduleSource::Migration)
    {
        // Assignment or a durable migration request is already committed.
        // A normal error would falsely imply rejection and could strand it.
        scheduler_invariant("committed migration notification", error);
    }
    outcome.status
}

fn scheduler_invariant(operation: &str, error: Error) -> ! {
    // Callers cross this boundary only after releasing the scheduler lock, so
    // coordinated crash-stop can freeze sibling CPUs and preserve snapshots.
    crate::diagnostics::crash::fatal(format_args!(
        "HypeR: {operation} invariant failed: {error:?}"
    ))
}

fn request_reschedule(cpu: CpuIndex, source: RescheduleSource) -> Result<(), Error> {
    if super::preempt::request(cpu, source)? {
        crate::irq::reschedule::notify(cpu);
    }
    Ok(())
}

fn prepare_schedule() -> Result<Option<PreparedTransition>, Error> {
    // SAFETY: This continuation transfers the exact prior interrupt state into
    // its saved ThreadContext at the final machine-switch boundary. Balancing
    // and CPU identification must follow masking to avoid a nested selection
    // or migration before this decision commits.
    let interrupt_mask = unsafe { TransitionMask::acquire() };
    balance::service();
    let cpu = current_cpu()?;
    let switch = match state::prepare_local_yield(cpu)? {
        state::LocalScheduleAttempt::Complete(switch) => switch,
        state::LocalScheduleAttempt::NeedsCoordinator => SCHEDULER.with(|slot| {
            let scheduler = slot.as_mut().ok_or(Error::NotInitialized)?;
            scheduler.prepare_yield(cpu)
        })?,
    };
    service_deadline_publication()?;
    Ok(switch.map(|switch| PreparedTransition {
        switch,
        interrupt_mask,
    }))
}

/// Retires one detached owner in normal, interruptible Thread context.
pub(crate) fn reap_one_thread(
    _access: &mut crate::reaper::ReaperAccess,
) -> Result<Option<bool>, Error> {
    let retired = SCHEDULER.with(|slot| {
        slot.as_mut()
            .ok_or(Error::NotInitialized)?
            .take_retirement()
    })?;
    let Some(state::RetiredThread {
        id,
        thread,
        retirement,
    }) = retired
    else {
        return Ok(None);
    };

    #[cfg(all(feature = "kernel-test", feature = "kernel-stack-audit"))]
    stack_audit::record_retired(&thread);
    #[cfg(feature = "kernel-trace")]
    let koid = thread.object_snapshot().object.koid.get();
    retire_detached_thread(thread);
    let more = SCHEDULER.with(|slot| {
        let scheduler = slot.as_mut().ok_or(Error::NotInitialized)?;
        scheduler.complete_retirement(id)?;
        Ok::<_, Error>(scheduler.has_retirements())
    })?;
    drop(retirement);
    // Execution resources and retirement publications are now complete. Keep
    // this distinct from the earlier Terminated switch-out observation.
    crate::tracepoint!(Scheduler, "sched_reap", thread = id.get(), koid = koid);
    Ok(Some(more))
}

/// Reports whether scheduler-detached execution awaits the shared reaper.
pub(crate) fn retirement_pending(_access: &crate::reaper::ReaperAccess) -> bool {
    read_scheduler(|scheduler| Ok(scheduler.has_retirements())).unwrap_or(false)
}

fn retire_detached_thread(mut thread: Box<Thread>) {
    if !thread.wait_context().is_idle() {
        hyper::debug::invariant_failure("scheduler retirement with live wait resources");
    }
    if let Some((object, execution)) = thread.take_user_execution() {
        drop(thread);
        execution.into_inner().complete_detach(object);
        return;
    }
    let vcpu_reap = thread.take_vcpu_reap_publication();
    drop(thread);
    if let Some(publication) = vcpu_reap
        && let Err(error) = publication.complete()
    {
        crate::diagnostics::crash::fatal(format_args!(
            "HypeR: exact vCPU reap publication failed: {error:?}"
        ));
    }
}

/// Terminates the current non-idle kernel Thread and schedules a successor.
pub(crate) fn exit_current() -> ! {
    kernel_thread_exit()
}

#[unsafe(no_mangle)]
extern "C" fn kernel_thread_exit() -> ! {
    // SAFETY: Mask before observing CPU/Thread identity. The exiting movable
    // continuation transfers this outer mask into its nonreturning switch.
    let interrupt_mask = unsafe { TransitionMask::acquire() };
    let cpu = match current_cpu() {
        Ok(cpu) => cpu,
        Err(error) => crate::diagnostics::crash::fatal(format_args!(
            "HypeR: thread exit on invalid CPU: {error:?}"
        )),
    };
    let thread = match current_thread_id() {
        Ok(thread) => thread,
        Err(error) => crate::diagnostics::crash::fatal(format_args!(
            "HypeR: thread exit without current identity: {error:?}"
        )),
    };
    let name = match thread_name(thread) {
        Ok(name) => name,
        Err(error) => crate::diagnostics::crash::fatal(format_args!(
            "HypeR: thread {thread:?} exit without name snapshot: {error:?}"
        )),
    };
    let result = SCHEDULER.with(|slot| {
        slot.as_mut()
            .ok_or(Error::NotInitialized)?
            .prepare_exit(cpu)
    });
    pr_debug!("HypeR: thread {thread:?} ({name}) exit");
    match result {
        Ok(pair) => {
            PreparedTransition {
                switch: pair,
                interrupt_mask,
            }
            .activate();
            crate::diagnostics::crash::fatal(format_args!(
                "HypeR: terminated thread context resumed unexpectedly"
            ));
        }
        Err(error) => {
            crate::diagnostics::crash::fatal(format_args!("HypeR: thread exit failed: {error:?}"))
        }
    }
}

fn current_cpu() -> Result<CpuIndex, Error> {
    crate::cpu::current_index().ok_or(Error::InvalidCpuIndex)
}

/// Completes source ownership on the incoming stack with interrupts masked.
extern "C" fn finish_context_switch_tail(ticket: usize) {
    let ticket = ticket as u64;
    let result =
        current_cpu().and_then(
            |cpu| match state::complete_local_switch_tail(cpu, ticket)? {
                state::LocalTailCompletion::Complete => Ok((None, false)),
                state::LocalTailCompletion::NeedsCoordinator => SCHEDULER.with(|slot| {
                    let completion = slot
                        .as_mut()
                        .ok_or(Error::NotInitialized)?
                        .complete_incoming_switch(cpu, ticket)?;
                    Ok((completion.ready, completion.retirement_published))
                }),
            },
        );
    match result {
        Ok((ready, retirement_published)) => {
            if let Some(outcome) = ready
                && let Err(error) = publish_ready_outcome(outcome)
            {
                crate::diagnostics::crash::fatal(format_args!(
                    "HypeR: switch-tail migration publication failed: {error:?}"
                ));
            }
            if retirement_published {
                crate::reaper::request();
            }
        }
        Err(error) => crate::diagnostics::crash::fatal(format_args!(
            "HypeR: incoming context-switch completion failed: {error:?}"
        )),
    }
}

fn service_deadline_publication() -> Result<(), Error> {
    crate::time::service_scheduler_deadline().map_err(|_| Error::PreemptionInvariant)
}
