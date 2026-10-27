// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Scheduler-owned thread registry, CPU state, and lifecycle transitions.
//!
//! This module owns the locks, shared transition primitives, and retirement.
//! Its private children group CPU-local decisions, migration, class changes,
//! and waiting without introducing another scheduler or publication boundary.

mod balance;
mod local;
mod migration;
mod policy;
mod runtime;
mod waiting;

pub(super) use local::{
    complete_local_switch_tail, local_current_thread, local_current_user,
    local_current_user_thread, local_current_vcpu, prepare_local_preemption, prepare_local_yield,
    try_cpu_snapshot,
};
#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
pub(super) use runtime::expire_current_turn;
pub(super) use runtime::{account_all_cpus, deadline_is_current};

use alloc::boxed::Box;
use hyper::cpu::{CpuIndex, PerCpu};
use hyper::sync::InterruptSpinLock;

use super::queue::{self, CpuRunQueue};
use super::registry::{
    CpuScheduleAuthorityToken, ThreadRegistry, ThreadReservation, ThreadTableCapability,
};
use super::switch_handoff::{SwitchDisposition, SwitchHandoff, SwitchingContext};
use super::trace::{SchedulePoint, SwitchReason};
use super::{Error, MigrationStatus, SecondaryStack, Statistics};
use {
    crate::task::policy::CpuMask, crate::task::policy::PlacementPolicy,
    crate::task::policy::SchedulingClass, crate::task::policy::SchedulingPolicy,
};
use {
    crate::task::thread::DeferredFifoPlacement, crate::task::thread::ExecutionKind,
    crate::task::thread::MigrationRequest, crate::task::thread::QueueMembership,
    crate::task::thread::Thread, crate::task::thread::ThreadId,
    crate::task::thread::ThreadScheduleState, crate::task::thread::ThreadState,
};
use {
    crate::task::wait::PendingResolution, crate::task::wait::ThreadQueue,
    crate::task::wait::WaitMobility, crate::task::wait::WaitOutcome, crate::task::wait::WaitQueue,
    crate::task::wait::WaitRecord, crate::task::wait::WaitRecordError,
    crate::task::wait::WaitTicket,
};

/// Owned observation produced by one closure-bounded table access.
///
/// It contains only copyable state and stable machine-resource addresses; no
/// Rust reference into a Thread slot can escape the authority closure.
#[derive(Clone, Copy)]
pub(super) struct ThreadObservation {
    schedule: Option<ThreadScheduleObservation>,
    execution: ExecutionKind,
    context: *mut crate::hal::context::ThreadContext,
}

#[derive(Clone, Copy)]
struct ThreadScheduleObservation {
    cpu: CpuIndex,
    affinity: CpuMask,
    placement: PlacementPolicy,
    state: ThreadState,
    scheduling: SchedulingPolicy,
    fair_slice_expired: bool,
    queue_links: crate::task::thread::QueueLinks,
    wait: WaitRecord,
    pending_migration: Option<MigrationRequest>,
}

impl ThreadObservation {
    fn capture(thread: &Thread) -> Self {
        let schedule_owner = thread.schedule_owner_cpu();
        Self {
            schedule: schedule_owner.is_none().then(|| ThreadScheduleObservation {
                cpu: thread.cpu_index(),
                affinity: thread.affinity(),
                placement: thread.placement_policy(),
                state: thread.state(),
                scheduling: thread.scheduling_policy(),
                fair_slice_expired: thread.fair_slice_expired(),
                queue_links: thread.queue_links(),
                wait: *thread.wait_record(),
                pending_migration: thread.pending_migration(),
            }),
            execution: thread.execution_kind(),
            context: thread.context_pointer(),
        }
    }

    fn capture_cpu(thread: &Thread, schedule: &ThreadScheduleState) -> Self {
        Self {
            schedule: Some(ThreadScheduleObservation {
                cpu: schedule.placement.assigned_cpu(),
                affinity: schedule.placement.affinity(),
                placement: schedule.placement.policy(),
                state: schedule.state,
                scheduling: schedule.scheduling,
                fair_slice_expired: schedule.fair_slice_expired(),
                // SAFETY: every Scheduler operation is serialized by the
                // TransitionLock in addition to this matching CPU lock.
                queue_links: unsafe { thread.combined_queue_links(schedule.ready_queue_links) },
                wait: schedule.wait,
                pending_migration: schedule.pending_migration,
            }),
            execution: thread.execution_kind(),
            context: thread.context_pointer(),
        }
    }

    fn schedule(self) -> ThreadScheduleObservation {
        self.schedule.unwrap_or_else(|| {
            hyper::debug::invariant_failure("task::scheduler::state::schedule invariant")
        })
    }

    fn cpu_index(self) -> CpuIndex {
        self.schedule().cpu
    }
    fn affinity(self) -> CpuMask {
        self.schedule().affinity
    }
    fn placement_policy(self) -> PlacementPolicy {
        self.schedule().placement
    }
    pub(super) fn state(self) -> ThreadState {
        self.schedule().state
    }
    fn scheduling_policy(self) -> SchedulingPolicy {
        self.schedule().scheduling
    }
    fn scheduling_class(self) -> SchedulingClass {
        self.schedule().scheduling.class()
    }
    fn fair_slice_expired(self) -> bool {
        self.schedule().fair_slice_expired
    }
    fn queue_links(self) -> crate::task::thread::QueueLinks {
        self.schedule().queue_links
    }
    fn wait_record(&self) -> &WaitRecord {
        match &self.schedule {
            Some(schedule) => &schedule.wait,
            None => {
                hyper::debug::invariant_failure("task::scheduler::state::wait_record invariant")
            }
        }
    }
    fn pending_migration(self) -> Option<MigrationRequest> {
        self.schedule().pending_migration
    }
    pub(super) const fn execution_kind(self) -> ExecutionKind {
        self.execution
    }
    const fn context_pointer(self) -> *mut crate::hal::context::ThreadContext {
        self.context
    }

    fn can_run_on(self, cpu: CpuIndex) -> bool {
        self.schedule().affinity.contains(cpu)
    }
}

/// Exclusive table authority scoped to one Thread identity without exposing a
/// reference to the underlying slot.
struct ThreadMutation<'registry> {
    registry: &'registry mut ThreadRegistry,
    id: ThreadId,
    cpu: Option<CpuIndex>,
}

impl ThreadMutation<'_> {
    fn apply<R>(&mut self, operation: impl for<'thread> FnOnce(&'thread mut Thread) -> R) -> R {
        match self.registry.with_thread_mut(self.id, operation) {
            Ok(value) => value,
            Err(_) => hyper::debug::invariant_failure("task::scheduler::state::apply invariant"),
        }
    }

    fn apply_schedule<R>(
        &mut self,
        operation: impl for<'schedule> FnOnce(&'schedule mut ThreadScheduleState) -> R,
    ) -> R {
        match self.cpu {
            None => self.apply(|thread| thread.with_coordinator_schedule_mut(operation)),
            Some(cpu) => match self.registry.with_thread(self.id, |thread| {
                // SAFETY: a CPU mutation is created only by `with_cpu_domain`,
                // after it has locked and revalidated this exact owner.
                unsafe { thread.with_cpu_schedule_mut(cpu, operation) }
            }) {
                Ok(Some(value)) => value,
                Ok(None) | Err(_) => hyper::debug::invariant_failure(
                    "task::scheduler::state::apply_schedule invariant",
                ),
            },
        }
    }

    fn arm_user_execution(&mut self, ownership: crate::process::UserExecutionOwnership) -> bool {
        self.apply(|thread| thread.arm_user_execution(ownership))
    }
    fn replace_affinity(&mut self, affinity: CpuMask) -> bool {
        self.apply_schedule(|schedule| {
            let Some(placement) = schedule.placement.with_affinity(affinity) else {
                return false;
            };
            schedule.placement = placement;
            true
        })
    }
    fn request_migration(&mut self, request: MigrationRequest) -> bool {
        self.apply_schedule(|schedule| match schedule.pending_migration {
            Some(existing) => existing == request,
            None => {
                schedule.pending_migration = Some(request);
                true
            }
        })
    }
    fn reassign_stopped_with_affinity(&mut self, cpu: CpuIndex, affinity: CpuMask) -> bool {
        self.apply_schedule(|schedule| {
            let Some(placement) = schedule.placement.reassign_with_affinity(cpu, affinity) else {
                return false;
            };
            schedule.placement = placement;
            true
        })
    }
    fn replenish_fair_slice(&mut self) {
        self.apply_schedule(|schedule| schedule.replenish_fair_slice());
    }
    fn set_deferred_fifo_placement(&mut self, placement: Option<DeferredFifoPlacement>) {
        self.apply_schedule(|schedule| schedule.deferred_fifo_placement = placement);
    }
    fn set_scheduling_policy(&mut self, policy: SchedulingPolicy) -> bool {
        self.apply_schedule(|schedule| {
            if schedule.scheduling_class() == SchedulingClass::Idle {
                return false;
            }
            schedule.install_parameters(super::default_parameters(policy));
            true
        })
    }
    fn set_state(&mut self, state: ThreadState) {
        self.apply_schedule(|schedule| schedule.state = state);
    }
    fn mark_running_on(&mut self, cpu: CpuIndex) -> bool {
        self.apply_schedule(|schedule| {
            let Some(placement) = schedule.placement.mark_running(cpu) else {
                return false;
            };
            schedule.placement = placement;
            schedule.deferred_fifo_placement = None;
            schedule.state = ThreadState::Running;
            schedule.auto_migrated = false;
            true
        })
    }
    fn take_migration_request(&mut self) -> Option<MigrationRequest> {
        self.apply_schedule(|schedule| schedule.pending_migration.take())
    }
    fn ensure_kernel_stack(&mut self) -> Result<(usize, usize), crate::task::thread::Error> {
        self.apply(Thread::ensure_kernel_stack)
    }
    fn become_idle(&mut self) {
        self.apply_schedule(|schedule| {
            schedule.install_parameters(super::default_parameters(SchedulingPolicy::Idle));
            schedule.state = ThreadState::Idle;
        });
    }
    fn with_wait_record<R>(
        &mut self,
        operation: impl for<'wait> FnOnce(&'wait mut WaitRecord) -> R,
    ) -> R {
        self.apply_schedule(|schedule| operation(&mut schedule.wait))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct ReadyOutcome {
    pub changed: bool,
    pub target_cpu: CpuIndex,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct MigrationOutcome {
    pub status: MigrationStatus,
    pub source_reschedule: Option<CpuIndex>,
    pub target_ready: Option<ReadyOutcome>,
}

pub(super) enum PreparedWait {
    Park {
        switch: PreparedContextSwitch,
        ticket: WaitTicket,
    },
    Completed(WaitOutcome),
}

pub(super) struct ResolvedWait {
    pub won: bool,
    pub ready: Option<ReadyOutcome>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum WakePreemption {
    Policy,
    FairBoundary,
}

pub(super) enum VcpuStopTarget {
    Running(CpuIndex),
    Runnable {
        cpu: CpuIndex,
        ready: Option<ReadyOutcome>,
    },
    Migrating(CpuIndex),
    Terminated,
}

pub(super) enum LocalScheduleAttempt {
    Complete(Option<PreparedContextSwitch>),
    NeedsCoordinator,
}

pub(super) enum LocalTailCompletion {
    Complete,
    NeedsCoordinator,
}

pub(super) struct CoordinatedTailCompletion {
    pub ready: Option<ReadyOutcome>,
    pub retirement_published: bool,
}

/// Detached resource owner consumed only by the dedicated reaper Thread.
pub(super) struct RetiredThread {
    pub id: ThreadId,
    pub thread: Box<Thread>,
    pub retirement: super::ResourceRetirement,
}

pub(super) struct Scheduler {
    // Slot backing grows outside locks; published segments never move.
    registry: ThreadRegistry,
    cpu_reservations: PerCpu<bool>,
    /// TransitionLock-owned admission truth for CPUs with an installed idle.
    ///
    /// This is policy metadata, not a mirror of CPU-local execution state.
    /// Cross-CPU placement may read it without nesting a target CPU lock.
    schedulable_cpus: CpuMask,
    /// Stack-scoped authority installed only while one `CpuScheduler` lock is held.
    active_domain: Option<ActiveCpuDomain>,
    /// Coordinator-owned entity awaiting publication after a source CPU lock
    /// is released. `TransitionLock` keeps this handoff invisible externally.
    deferred_ready_handoff: Option<(ThreadId, CpuIndex)>,
    deferred_blocked_handoff: Option<(ThreadId, CpuIndex)>,
    /// Bounded maintenance resumes here even when a prefix contains only
    /// pinned, blocked or otherwise ineligible Threads.
    balance_cursor: balance::BalanceCursor,
    /// Exclusively borrowed by coordinator-owned placement operations.
    balance_workspace: Option<Box<balance::BalanceWorkspace>>,
    terminated: ThreadQueue,
    retirement_head: Option<ThreadId>,
    retirement_tail: Option<ThreadId>,
}

// SAFETY: registry slots, residence changes and coordinator metadata are
// mutated only with all reader lanes held. Shared operations pin those slots
// and use CPU locks for schedule cells and WaitQueue locks for control links.
// The stack-scoped active_domain is installed only during exclusive access.
unsafe impl Sync for Scheduler {}

/// CPU-local scheduling state that may be touched without the transition lock.
///
/// `current` is a linear token: it is either here or temporarily returned to
/// its registry Thread while a transition holds this CPU lock.
struct CpuScheduler {
    index: CpuIndex,
    table: ThreadTableCapability,
    authority: CpuScheduleAuthorityToken,
    current: ThreadId,
    idle: Option<ThreadId>,
    run_queue: CpuRunQueue,
    handoff: SwitchHandoff<ThreadId>,
    last_account_ns: Option<u64>,
    window: super::algorithms::arbitration::CpuWindow,
    dispatch_mode: super::algorithms::arbitration::DispatchMode,
    deadline_generation: u64,
    armed_deadline_ns: Option<u64>,
    fair_floor: u128,
}

#[derive(Clone, Copy)]
struct ActiveCpuDomain {
    cpu: CpuIndex,
    local: core::ptr::NonNull<CpuScheduler>,
}

// SAFETY: this pointer is never transferred as an independent capability. It
// is installed and cleared while SCHEDULER and the pointed-to CPU lock are
// both held, and every dereference revalidates the recorded CPU.
unsafe impl Send for ActiveCpuDomain {}

type CpuSchedulerLock = InterruptSpinLock<Option<CpuScheduler>, crate::hal::irq::LocalMask>;

static CPU_SCHEDULERS: PerCpu<CpuSchedulerLock> =
    PerCpu::new([const { CpuSchedulerLock::new(None) }; hyper::cpu::MAX_CPUS]);

#[must_use = "a prepared context switch must be consumed by the architecture boundary"]
pub(super) struct PreparedContextSwitch {
    previous: *mut crate::hal::context::ThreadContext,
    next: *const crate::hal::context::ThreadContext,
    ticket: u64,
    armed: bool,
}

impl PreparedContextSwitch {
    /// Activates the only context switch represented by this transition.
    ///
    /// The scheduler transaction is already committed, so this is the sole
    /// operation which can disarm the fail-stop Drop path.
    pub(super) fn activate(mut self, previous_interrupt_state: super::TransitionMaskState) {
        self.armed = false;
        // SAFETY: Scheduler queues contain only pinned, scheduler-owned
        // Threads. The assembly path saves `previous_interrupt_state`, changes
        // to the incoming stack, and completes switching_from ownership before
        // it restores the incoming interrupt state.
        unsafe {
            crate::hal::context::switch_thread_context(
                self.previous,
                self.next,
                previous_interrupt_state,
                super::finish_context_switch_tail,
                self.ticket as usize,
            )
        };
    }
}

impl Drop for PreparedContextSwitch {
    fn drop(&mut self) {
        if self.armed {
            // Drop may run while scheduler or architecture transition locks
            // are held. Ordinary logs could deadlock before preserving the
            // committed context-switch state.
            hyper::debug::invariant_failure("task::scheduler::state::drop invariant")
        }
    }
}

impl Scheduler {
    #[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
    pub fn thread_object_snapshot(
        &self,
        id: ThreadId,
    ) -> Result<crate::task::ThreadObjectSnapshot, Error> {
        self.registry.object_snapshot(id)
    }

    pub fn scan_thread_objects(
        &self,
        cursor: crate::task::ThreadObjectScanCursor,
        output: &mut crate::task::ThreadObjectSnapshotPage,
    ) {
        self.registry.scan_objects(cursor, output)
    }

    /// Routes schedule access without recursively reacquiring an active CPU lock.
    ///
    /// `Some(cpu)` means the caller must enter that CPU domain. `None` means
    /// either coordinator ownership or that the matching CPU domain is
    /// already active and the function body may proceed directly.
    fn cpu_lock_required_for(&self, id: ThreadId) -> Result<Option<CpuIndex>, Error> {
        let owner = self.registry.with_thread(id, Thread::schedule_owner_cpu)?;
        match (owner, self.active_domain) {
            (None, _) => Ok(None),
            (Some(cpu), None) => Ok(Some(cpu)),
            (Some(cpu), Some(active)) if cpu == active.cpu => Ok(None),
            (Some(_), Some(_)) => Err(Error::InvalidThreadState),
        }
    }

    pub fn new(boot_cpu: CpuIndex) -> Result<Self, Error> {
        let balance_workspace =
            hyper::mm::try_box(balance::BalanceWorkspace::new()).map_err(|_| Error::Allocation)?;
        let bootstrap =
            hyper::mm::try_box(Thread::bootstrap(boot_cpu)?).map_err(|_| Error::Allocation)?;
        let boot_idle_id = match ThreadId::from_scheduler_parts(1, 1) {
            Some(id) => id,
            None => return Err(Error::IdentifierExhausted),
        };
        let boot_idle = Thread::idle(boot_idle_id, boot_cpu, super::idle_thread_entry)?;
        let boot_idle = hyper::mm::try_box(boot_idle).map_err(|_| Error::Allocation)?;
        let registry = ThreadRegistry::new(bootstrap, boot_idle)?;
        Ok(Self {
            registry,
            cpu_reservations: PerCpu::new([false; hyper::cpu::MAX_CPUS]),
            schedulable_cpus: CpuMask::EMPTY,
            active_domain: None,
            deferred_ready_handoff: None,
            deferred_blocked_handoff: None,
            balance_cursor: balance::BalanceCursor::new(),
            balance_workspace: Some(balance_workspace),
            terminated: ThreadQueue::new(),
            retirement_head: None,
            retirement_tail: None,
        })
    }

    /// Publishes the boot CPU's linear running token during global commit.
    pub fn activate_boot_cpu(
        &mut self,
        cpu: CpuIndex,
        scratch: &mut Option<queue::QueueScratch>,
    ) -> Result<(), Error> {
        if scratch.is_none() {
            return Err(Error::InvalidThreadState);
        }
        if !CPU_SCHEDULERS[cpu].with(|slot| slot.is_none()) {
            return Err(Error::AlreadyInitialized);
        }
        let claimed = self
            .registry
            .with_thread_mut(ThreadId::BOOTSTRAP, |thread| thread.claim_schedule(cpu))?;
        let boot_idle = ThreadId::from_scheduler_parts(1, 1).ok_or(Error::IdentifierExhausted)?;
        let idle_claimed = self
            .registry
            .with_thread_mut(boot_idle, |thread| thread.claim_schedule(cpu))?;
        if !claimed || !idle_claimed {
            return Err(Error::InvalidThreadState);
        }
        // All fallible scheduler construction is complete. From this point the
        // stable directory has kernel lifetime and can later be addressed through
        // transferred CPU authority tokens without borrowing `Scheduler`.
        self.registry.publish_table();
        let table = self.registry.published_table();
        let installed = CPU_SCHEDULERS[cpu].with(|slot| {
            if slot.is_some() {
                return false;
            }
            let scratch = scratch
                .take()
                .unwrap_or_else(|| scheduler_invariant(Error::InvalidThreadState));
            let mut scheduler = CpuScheduler::new(cpu, table, ThreadId::BOOTSTRAP, scratch);
            scheduler.idle = Some(boot_idle);
            *slot = Some(scheduler);
            true
        });
        if !installed {
            hyper::debug::invariant_failure("task::scheduler::state::activate_boot_cpu invariant");
        }
        self.schedulable_cpus = self.schedulable_cpus.with_cpu(cpu);
        Ok(())
    }

    pub fn current_thread(&self, cpu: CpuIndex) -> Result<ThreadId, Error> {
        if let Some(active) = self.active_domain {
            if active.cpu != cpu {
                return Err(Error::CpuNotRegistered);
            }
            // SAFETY: active_domain exists only inside the matching lock's
            // closure and is cleared before that closure returns.
            return Ok(unsafe { active.local.as_ref() }.current);
        }
        CPU_SCHEDULERS[cpu].with(|slot| {
            slot.as_ref()
                .map(|scheduler| scheduler.current)
                .ok_or(Error::CpuNotRegistered)
        })
    }

    pub fn thread_placement(&mut self, id: ThreadId) -> Result<(CpuIndex, CpuMask), Error> {
        if let Some(cpu) = self.cpu_lock_required_for(id)? {
            return self.with_cpu_schedule_stored(cpu, |scheduler| scheduler.thread_placement(id));
        }
        let thread = self.thread(id)?;
        Ok((thread.cpu_index(), thread.affinity()))
    }

    pub fn thread_migration_state(
        &mut self,
        id: ThreadId,
    ) -> Result<(CpuIndex, Option<CpuIndex>), Error> {
        if let Some(cpu) = self.cpu_lock_required_for(id)? {
            return self
                .with_cpu_schedule_stored(cpu, |scheduler| scheduler.thread_migration_state(id));
        }
        let thread = self.thread(id)?;
        Ok((
            thread.cpu_index(),
            thread.pending_migration().map(|request| request.target),
        ))
    }

    pub fn stack_statistics(
        &mut self,
        id: ThreadId,
    ) -> Result<Option<crate::mm::stack::StackStatistics>, Error> {
        if let Some(cpu) = self.cpu_lock_required_for(id)? {
            return self.with_cpu_schedule_stored(cpu, |scheduler| scheduler.stack_statistics(id));
        }
        if let Some(active) = self.active_domain {
            if self.thread(id)?.state() != ThreadState::Blocked
                || self
                    .switching_from(active.cpu)?
                    .is_some_and(|switching| switching.thread == id)
            {
                return Err(Error::InvalidThreadState);
            }
        } else if self.thread(id)?.state() == ThreadState::Migrating
            || !self.context_is_stopped(id)?
        {
            return Err(Error::InvalidThreadState);
        }
        self.with_thread(id, Thread::kernel_stack_statistics)
    }

    /// Reports whether no CPU owns or may still be saving this context.
    pub fn context_is_stopped(&self, id: ThreadId) -> Result<bool, Error> {
        let _ = self.thread(id)?;
        if self.active_domain.is_some() {
            // Never acquire a second CPU lock from an active CPU domain.
            // Conservatively defer reclamation until the outer transition
            // releases its owner lock.
            return Ok(false);
        }
        for index in 0..hyper::cpu::MAX_CPUS {
            let Some(cpu) = CpuIndex::new(index) else {
                hyper::debug::invariant_failure(
                    "task::scheduler::state::context_is_stopped invariant",
                );
            };
            let in_use = CPU_SCHEDULERS[cpu].with(|slot| {
                slot.as_ref().is_some_and(|local| {
                    local.current == id
                        || local
                            .handoff
                            .current()
                            .is_some_and(|switching| switching.thread == id)
                })
            });
            if in_use {
                return Ok(false);
            }
        }
        Ok(true)
    }

    pub fn reserve_secondary(&mut self, cpu: CpuIndex) -> Result<ThreadReservation, Error> {
        if CPU_SCHEDULERS[cpu].with(|slot| slot.is_some()) || self.cpu_reservations[cpu] {
            return Err(Error::CpuAlreadyRegistered);
        }
        let reservation = self.reserve_thread_slot(cpu)?;
        self.cpu_reservations[cpu] = true;
        Ok(reservation)
    }

    pub fn publish_secondary(
        &mut self,
        reservation: &ThreadReservation,
        thread: Box<Thread>,
        scratch: &mut Option<queue::QueueScratch>,
    ) -> Result<SecondaryStack, (Error, Box<Thread>)> {
        if scratch.is_none()
            || !self.cpu_reservations[reservation.cpu()]
            || CPU_SCHEDULERS[reservation.cpu()].with(|slot| slot.is_some())
        {
            return Err((Error::InvalidThreadState, thread));
        }
        if !CPU_SCHEDULERS[reservation.cpu()].with(|slot| slot.is_none()) {
            return Err((Error::CpuAlreadyRegistered, thread));
        }
        let Some(virtual_top) = thread.kernel_stack_top() else {
            return Err((Error::Allocation, thread));
        };
        let Some(physical_top) = thread.kernel_stack_physical_top() else {
            return Err((Error::Allocation, thread));
        };
        self.publish_thread(reservation, thread)?;
        let claimed = match self.registry.with_thread_mut(reservation.id(), |thread| {
            thread.claim_schedule(reservation.cpu())
        }) {
            Ok(claimed) => claimed,
            Err(error) => scheduler_invariant(error),
        };
        if !claimed {
            scheduler_invariant(Error::InvalidThreadState);
        }
        let table = self.registry.published_table();
        let installed = CPU_SCHEDULERS[reservation.cpu()].with(|slot| {
            if slot.is_some() {
                return false;
            }
            *slot = Some(CpuScheduler::new(
                reservation.cpu(),
                table,
                reservation.id(),
                scratch
                    .take()
                    .unwrap_or_else(|| scheduler_invariant(Error::InvalidThreadState)),
            ));
            true
        });
        if !installed {
            hyper::debug::invariant_failure("task::scheduler::state::publish_secondary invariant");
        }
        self.cpu_reservations[reservation.cpu()] = false;
        Ok(SecondaryStack {
            physical_top,
            virtual_top,
        })
    }

    pub fn abandon_reservation(&mut self, reservation: &ThreadReservation) -> Result<(), Error> {
        // Validate and release the exact registry capability before changing
        // secondary-CPU admission state. A stale reservation must be entirely
        // failure-atomic.
        self.registry.abandon(reservation)?;
        if self.cpu_reservations[reservation.cpu()] {
            self.cpu_reservations[reservation.cpu()] = false;
        }
        Ok(())
    }

    pub fn reserve_kernel_thread(
        &mut self,
        preferred_cpu: CpuIndex,
        affinity: CpuMask,
        policy: SchedulingPolicy,
    ) -> Result<ThreadReservation, Error> {
        let cpu = if policy == SchedulingPolicy::Fair {
            self.select_initial_cpu(preferred_cpu, affinity)?
        } else {
            self.select_cpu(preferred_cpu, affinity)?
        };
        self.reserve_thread_slot(cpu)
    }

    /// Selects an admitted CPU without inspecting any CPU-owned schedule.
    ///
    /// If the preferred CPU is unavailable or excluded, the lowest-numbered
    /// registered CPU in the mask provides a stable fallback. Affinity changes
    /// may call this while holding a source CPU lock; FIFO initial placement
    /// also retains this locality policy rather than participating in balancing.
    fn select_cpu(&self, preferred_cpu: CpuIndex, affinity: CpuMask) -> Result<CpuIndex, Error> {
        if affinity.is_empty() {
            return Err(Error::EmptyCpuAffinity);
        }
        if affinity.contains(preferred_cpu) && self.cpu_is_schedulable(preferred_cpu) {
            return Ok(preferred_cpu);
        }
        for index in 0..hyper::cpu::MAX_CPUS {
            let Some(cpu) = CpuIndex::new(index) else {
                hyper::debug::invariant_failure("task::scheduler::state::select_cpu invariant");
            };
            if affinity.contains(cpu) && self.cpu_is_schedulable(cpu) {
                return Ok(cpu);
            }
        }
        Err(Error::NoRegisteredCpuInAffinity)
    }

    pub(super) fn validate_affinity(
        &self,
        preferred_cpu: CpuIndex,
        affinity: CpuMask,
    ) -> Result<(), Error> {
        self.select_cpu(preferred_cpu, affinity).map(|_| ())
    }

    /// Spreads one immutable guest topology over currently admitted host CPUs.
    /// The boot vCPU retains creator locality; subsequent members wrap through
    /// schedulable CPUs. Readiness itself does not rebalance dormant Threads,
    /// so assigning every member to the creator would serialize guest SMP.
    pub fn reserve_vcpu_thread(
        &mut self,
        preferred: CpuIndex,
        topology_index: u32,
    ) -> Result<ThreadReservation, Error> {
        let count = (0..hyper::cpu::MAX_CPUS)
            .filter_map(CpuIndex::new)
            .filter(|cpu| self.cpu_is_schedulable(*cpu))
            .count();
        if count == 0 {
            return Err(Error::NoRegisteredCpuInAffinity);
        }
        let ordinal = topology_index as usize % count;
        let cpu = (0..hyper::cpu::MAX_CPUS)
            .filter_map(|offset| CpuIndex::new((preferred.get() + offset) % hyper::cpu::MAX_CPUS))
            .filter(|cpu| self.cpu_is_schedulable(*cpu))
            .nth(ordinal)
            .ok_or(Error::NoRegisteredCpuInAffinity)?;
        self.reserve_thread_slot(cpu)
    }

    pub fn publish_thread(
        &mut self,
        reservation: &ThreadReservation,
        thread: Box<Thread>,
    ) -> Result<(), (Error, Box<Thread>)> {
        self.registry.publish(reservation, thread)
    }

    pub fn take_dormant_vcpu(&mut self, id: ThreadId) -> Result<Box<Thread>, Error> {
        self.take_dormant_thread(id, crate::task::thread::ExecutionKind::Vcpu)
    }

    pub fn take_dormant_user(&mut self, id: ThreadId) -> Result<Box<Thread>, Error> {
        self.take_dormant_thread(id, crate::task::thread::ExecutionKind::User)
    }

    pub fn arm_dormant_user(
        &mut self,
        id: ThreadId,
        ownership: crate::process::UserExecutionOwnership,
    ) -> Result<(), Error> {
        if self.thread(id)?.state() != ThreadState::Dormant {
            return Err(Error::InvalidThreadState);
        }
        let mut thread = self.thread_mut(id)?;
        if !thread.arm_user_execution(ownership) {
            return Err(Error::InvalidThreadState);
        }
        Ok(())
    }

    #[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
    pub fn take_dormant_kernel_thread(&mut self, id: ThreadId) -> Result<Box<Thread>, Error> {
        self.take_dormant_thread(id, crate::task::thread::ExecutionKind::Kernel)
    }

    fn take_dormant_thread(
        &mut self,
        id: ThreadId,
        execution: crate::task::thread::ExecutionKind,
    ) -> Result<Box<Thread>, Error> {
        let valid = self.registry.with_thread(id, |thread| {
            thread.id() == id
                && thread.state() == ThreadState::Dormant
                && thread.queue_links().membership == QueueMembership::None
                && thread.execution_kind() == execution
        })?;
        if !valid {
            return Err(Error::InvalidThreadState);
        }
        self.registry.take(id)
    }

    /// Returns the CPU currently executing the exact vCPU Thread.
    ///
    /// Ready, dormant, blocked, and migrating Threads deliberately have no
    /// prompt target. Their next activation consumes durable VM work instead
    /// of guessing from a mutable placement assignment.
    pub fn running_vcpu_cpu(&self, id: ThreadId) -> Result<Option<CpuIndex>, Error> {
        let thread = self.thread(id)?;
        if thread.execution_kind() != crate::task::thread::ExecutionKind::Vcpu {
            return Err(Error::InvalidThreadState);
        }
        if let Some(cpu) = self.cpu_lock_required_for(id)? {
            // The registry reader pins residence; the CPU lock supplies
            // a coherent running-state observation.
            return CPU_SCHEDULERS[cpu].with(|slot| {
                let local = slot.as_mut().ok_or(Error::CpuNotRegistered)?;
                let current = local.current;
                let threads = local.thread_authority();
                threads.with_thread(id, |_thread, schedule| match schedule.state {
                    ThreadState::Running if current == id => Ok(Some(cpu)),
                    ThreadState::Running => Err(Error::InvalidThreadState),
                    ThreadState::Ready
                    | ThreadState::Dormant
                    | ThreadState::Blocked
                    | ThreadState::Migrating
                    | ThreadState::Terminated => Ok(None),
                    ThreadState::Idle => Err(Error::InvalidThreadState),
                })?
            });
        }
        match thread.state() {
            ThreadState::Running => {
                let cpu = thread.cpu_index();
                (self.current_thread(cpu)? == id)
                    .then_some(Some(cpu))
                    .ok_or(Error::InvalidThreadState)
            }
            ThreadState::Dormant
            | ThreadState::Ready
            | ThreadState::Blocked
            | ThreadState::Migrating
            | ThreadState::Terminated => Ok(None),
            ThreadState::Idle => Err(Error::InvalidThreadState),
        }
    }

    /// Classifies the exact vCPU continuation after durable stop publication.
    ///
    /// A blocked WFI continuation must already have been resolved through its
    /// endpoint ticket before this transaction. Dormant vCPUs become runnable
    /// so the fixed runner can publish hardware-detached and reap ownership.
    pub fn request_vcpu_stop(&mut self, id: ThreadId) -> Result<VcpuStopTarget, Error> {
        if let Some(cpu) = self.cpu_lock_required_for(id)? {
            return self.with_cpu_schedule_stored(cpu, |scheduler| scheduler.request_vcpu_stop(id));
        }
        let thread = self.thread(id)?;
        if thread.execution_kind() != ExecutionKind::Vcpu {
            return Err(Error::InvalidThreadState);
        }
        let cpu = thread.cpu_index();
        match thread.state() {
            ThreadState::Dormant => {
                let ready = self.make_ready(id)?;
                Ok(VcpuStopTarget::Runnable {
                    cpu,
                    ready: Some(ready),
                })
            }
            ThreadState::Ready => Ok(VcpuStopTarget::Runnable { cpu, ready: None }),
            ThreadState::Running => {
                if self.current_thread(cpu)? != id {
                    return Err(Error::InvalidThreadState);
                }
                Ok(VcpuStopTarget::Running(cpu))
            }
            ThreadState::Migrating => Ok(VcpuStopTarget::Migrating(cpu)),
            ThreadState::Terminated => Ok(VcpuStopTarget::Terminated),
            ThreadState::Blocked | ThreadState::Idle => Err(Error::InvalidThreadState),
        }
    }

    /// Validate lifecycle and scheduling state under the same owner CPU lock.
    /// A user start request must never restart a blocked wait registration.
    pub fn make_user_ready(&mut self, id: ThreadId) -> Result<ReadyOutcome, Error> {
        if let Some(cpu) = self.cpu_lock_required_for(id)? {
            return self.with_cpu_schedule_stored(cpu, |scheduler| scheduler.make_user_ready(id));
        }
        if !matches!(
            self.thread(id)?.state(),
            ThreadState::Dormant | ThreadState::Ready | ThreadState::Running
        ) {
            return Err(Error::InvalidThreadState);
        }
        self.with_thread(id, |thread| {
            thread
                .user_thread()
                .ok_or(Error::InvalidThreadState)?
                .mark_runnable()
                .map_err(|_| Error::InvalidThreadState)
        })??;
        // State cannot change while its scheduler owner is held. An error here
        // would indicate internal queue corruption after lifecycle publication.
        match self.make_ready(id) {
            Ok(outcome) => Ok(outcome),
            Err(_) => {
                hyper::debug::invariant_failure("task::scheduler::state::make_user_ready invariant")
            }
        }
    }

    pub fn make_ready(&mut self, id: ThreadId) -> Result<ReadyOutcome, Error> {
        if let Some(cpu) = self.cpu_lock_required_for(id)? {
            return self.with_cpu_schedule_stored(cpu, |scheduler| scheduler.make_ready(id));
        }
        let cpu = self.thread(id)?.cpu_index();
        match self.thread(id)?.state() {
            ThreadState::Dormant => {
                self.enqueue_ready(id)?;
                Ok(ReadyOutcome {
                    changed: true,
                    target_cpu: cpu,
                })
            }
            ThreadState::Blocked => Err(Error::ThreadBlocked),
            ThreadState::Ready | ThreadState::Running | ThreadState::Idle => Ok(ReadyOutcome {
                changed: false,
                target_cpu: cpu,
            }),
            ThreadState::Migrating => Err(Error::MigrationInProgress),
            ThreadState::Terminated => Err(Error::TerminatedThread),
        }
    }

    pub fn make_ready_from_wait(&mut self, id: ThreadId) -> Result<ReadyOutcome, Error> {
        let thread = self.thread(id)?;
        if thread.state() != ThreadState::Blocked
            || thread.queue_links().membership != QueueMembership::None
        {
            return Err(Error::QueueCorrupted);
        }
        let cpu = thread.cpu_index();
        self.enqueue_ready(id)?;
        Ok(ReadyOutcome {
            changed: true,
            target_cpu: cpu,
        })
    }

    /// Selects work made eligible by a class preemption or Fair slice expiry.
    ///
    /// A preempted FIFO thread is returned to the head of its own priority
    /// queue. Equal-priority peers therefore cannot overtake it merely because
    /// a higher-priority thread ran temporarily.
    pub fn prepare_preemption(
        &mut self,
        cpu: CpuIndex,
        point: SchedulePoint,
    ) -> Result<Option<PreparedContextSwitch>, Error> {
        self.prepare_policy_decision(cpu, false, point)
    }

    pub fn prepare_yield(&mut self, cpu: CpuIndex) -> Result<Option<PreparedContextSwitch>, Error> {
        self.prepare_policy_decision(cpu, true, SchedulePoint::Explicit)
    }

    fn prepare_policy_decision(
        &mut self,
        cpu: CpuIndex,
        yielding: bool,
        point: SchedulePoint,
    ) -> Result<Option<PreparedContextSwitch>, Error> {
        let current = self.current_thread(cpu)?;
        if let Some(owner) = self.cpu_lock_required_for(current)? {
            return self.with_cpu_schedule_stored(owner, |scheduler| {
                scheduler.prepare_policy_decision(cpu, yielding, point)
            });
        }
        let _ = super::super::preempt::take_pending_locked(cpu)?;
        if self.thread(current)?.pending_migration().is_some() {
            return self
                .prepare_running_migration(cpu, current, point)
                .map(Some);
        }
        let choice = self.with_cpu_domain(cpu, |_scheduler, local| local.choose_ready(yielding))?;
        let Some((next, front)) = choice else {
            return Ok(None);
        };
        let current_state = self.thread(current)?.state();
        let reason = if current_state == ThreadState::Idle {
            SwitchReason::Idle
        } else if yielding {
            SwitchReason::Yield
        } else {
            SwitchReason::Preempt
        };
        let membership = self.thread(next)?.queue_links().membership;
        self.remove_ready(next, cpu, membership)?;
        if current_state == ThreadState::Running {
            if front {
                self.enqueue_ready_front(current)?;
            } else {
                self.enqueue_ready(current)?;
            }
        }
        self.prepare_switch(cpu, current, next, reason, point)
            .map(Some)
    }

    pub fn resolve_wait(
        &mut self,
        ticket: WaitTicket,
        outcome: WaitOutcome,
    ) -> Result<ResolvedWait, Error> {
        self.resolve_wait_with(ticket, outcome, || {})
    }

    /// Resolves one exact wait generation and commits resolver-owned state.
    ///
    /// `on_commit` runs only for the winning resolver, after the wait record
    /// and any queue membership have been completed but before a blocked
    /// Thread is published Ready. The callback must be bounded, infallible,
    /// allocation-free, and must not re-enter the scheduler.
    pub fn resolve_wait_with(
        &mut self,
        ticket: WaitTicket,
        outcome: WaitOutcome,
        on_commit: impl FnOnce(),
    ) -> Result<ResolvedWait, Error> {
        self.resolve_wait_with_preemption(ticket, outcome, WakePreemption::Policy, on_commit)
    }

    /// Resolves one wait with an optional equal-Fair scheduling boundary.
    ///
    /// A Fair boundary expires the running Fair entity only after a blocked
    /// Fair waiter becomes Ready. Real-time class ordering remains entirely
    /// governed by [`SchedulingPolicy`].
    pub fn resolve_wait_with_preemption(
        &mut self,
        ticket: WaitTicket,
        outcome: WaitOutcome,
        preemption: WakePreemption,
        on_commit: impl FnOnce(),
    ) -> Result<ResolvedWait, Error> {
        if let Ok(Some(cpu)) = self.cpu_lock_required_for(ticket.thread()) {
            return self.with_cpu_schedule_stored(cpu, |scheduler| {
                scheduler.resolve_wait_with_preemption(ticket, outcome, preemption, on_commit)
            });
        }
        let pending = match self.thread(ticket.thread()) {
            Ok(thread) => thread.wait_record().pending_resolution(ticket)?,
            Err(Error::ThreadNotFound) => {
                return Ok(ResolvedWait {
                    won: false,
                    ready: None,
                });
            }
            Err(error) => return Err(error),
        };
        match pending {
            PendingResolution::Stale | PendingResolution::AlreadyCompleted => Ok(ResolvedWait {
                won: false,
                ready: None,
            }),
            PendingResolution::Armed => {
                let mut thread = match self.thread_mut(ticket.thread()) {
                    Ok(thread) => thread,
                    Err(error) => scheduler_invariant(error),
                };
                let completion = thread.with_wait_record(|wait| wait.complete(ticket, outcome));
                if completion.is_err() {
                    scheduler_invariant(Error::InvalidWaitRegistration);
                }
                on_commit();
                Ok(ResolvedWait {
                    won: true,
                    ready: None,
                })
            }
            PendingResolution::Queued { queue } => {
                // SAFETY: a queued wait owns a live reference to its WaitQueue
                // until the same scheduler-lock transaction unlinks it.
                let wait_queue =
                    unsafe { &*core::ptr::with_exposed_provenance::<WaitQueue>(queue) };
                if let Err(error) = self.remove_waiter(wait_queue, ticket.thread()) {
                    scheduler_invariant(error);
                }
                let mut thread = match self.thread_mut(ticket.thread()) {
                    Ok(thread) => thread,
                    Err(error) => scheduler_invariant(error),
                };
                let completion = thread.with_wait_record(|wait| wait.complete(ticket, outcome));
                if completion.is_err() {
                    scheduler_invariant(Error::InvalidWaitRegistration);
                }
                on_commit();
                let ready = match self.make_ready_from_wait(ticket.thread()) {
                    Ok(ready) => ready,
                    Err(error) => scheduler_invariant(error),
                };
                if preemption == WakePreemption::FairBoundary {
                    self.expire_current_for_fair_wakeup(ticket.thread())
                        .unwrap_or_else(|error| scheduler_invariant(error));
                }
                Ok(ResolvedWait {
                    won: true,
                    ready: Some(ready),
                })
            }
        }
    }

    pub fn prepare_exit(&mut self, cpu: CpuIndex) -> Result<PreparedContextSwitch, Error> {
        let current = self.current_thread(cpu)?;
        if let Some(owner) = self.cpu_lock_required_for(current)? {
            return self.with_cpu_schedule_stored(owner, |scheduler| scheduler.prepare_exit(cpu));
        }
        let cpu_slot = self.cpu_slot(cpu)?;
        let current = self.current_thread(cpu_slot)?;
        if !self.thread(current)?.wait_record().is_idle()
            || !self.with_thread(current, |thread| thread.wait_context().is_idle())?
        {
            return Err(Error::InvalidWaitRegistration);
        }
        // Validate a distinct successor before publishing termination. Once
        // the current Thread enters the terminated queue, every later failure
        // is an internal scheduler invariant and cannot be rolled back.
        let successor = self
            .peek_ready(cpu_slot)?
            .map(|ready| ready.id)
            .or(self.idle_thread(cpu_slot)?)
            .ok_or(Error::CurrentThreadMissing)?;
        if successor == current {
            return Err(Error::CurrentThreadMissing);
        }
        Self::queue_push(
            &mut self.registry,
            &mut self.terminated,
            current,
            QueueMembership::Terminated,
        )?;
        self.thread_mut(current)?.set_state(ThreadState::Terminated);
        let next = self
            .dequeue_ready(cpu_slot)?
            .or(self.idle_thread(cpu_slot)?)
            .ok_or(Error::CurrentThreadMissing)?;
        if next != successor {
            scheduler_invariant(Error::QueueCorrupted);
        }
        self.prepare_switch(
            cpu_slot,
            current,
            next,
            SwitchReason::Exit,
            SchedulePoint::Explicit,
        )
    }

    pub fn request_user_stop(
        &mut self,
        id: ThreadId,
        reason: crate::process::TerminalReason,
    ) -> Result<Option<CpuIndex>, Error> {
        if let Some(cpu) = self.cpu_lock_required_for(id)? {
            return self.with_cpu_schedule_stored(cpu, |scheduler| {
                scheduler.request_user_stop(id, reason)
            });
        }
        self.with_thread(id, |thread| {
            let object = thread.user_thread().ok_or(Error::InvalidThreadState)?;
            object.request_stop(reason);
            Ok::<_, Error>(())
        })??;
        // `resolve_wait` may publish a blocked Thread directly into a CPU
        // ready domain. Capture every coordinator decision input before that
        // linear ownership transfer; no stored-schedule observation is valid
        // after a queued resolution returns `ready`.
        let (state, cpu, ticket) = {
            let thread = self.thread(id)?;
            (
                thread.state(),
                thread.cpu_index(),
                thread.wait_record().current_ticket(id),
            )
        };
        let wait_resolution = match ticket {
            Some(ticket) => Some(self.resolve_wait(ticket, WaitOutcome::Cancelled)?),
            None => None,
        };
        match state {
            ThreadState::Running => Ok(Some(cpu)),
            ThreadState::Dormant => {
                self.enqueue_terminated(id)?;
                Ok(Some(cpu))
            }
            ThreadState::Ready => {
                // The suspended continuation may own arbitrary RAII state.
                // Keep it queued so the fixed user runner can unwind normally.
                Ok(Some(cpu))
            }
            ThreadState::Blocked => {
                let resolved = wait_resolution.ok_or(Error::InvalidWaitRegistration)?;
                let ready = resolved.ready.ok_or(Error::InvalidWaitRegistration)?;
                Ok(Some(ready.target_cpu))
            }
            ThreadState::Migrating => {
                // Source context is still owned until switch-tail. The durable
                // stop state follows the Thread to its target continuation.
                Ok(Some(cpu))
            }
            ThreadState::Terminated => Ok(None),
            ThreadState::Idle => Err(Error::InvalidThreadState),
        }
    }

    fn enqueue_terminated(&mut self, id: ThreadId) -> Result<(), Error> {
        if !self.with_thread(id, |thread| thread.wait_context().is_idle())? {
            return Err(Error::InvalidWaitRegistration);
        }
        if self.thread(id)?.queue_links().membership != QueueMembership::None {
            return Err(Error::QueueCorrupted);
        }
        Self::queue_push(
            &mut self.registry,
            &mut self.terminated,
            id,
            QueueMembership::Terminated,
        )?;
        self.thread_mut(id)?.set_state(ThreadState::Terminated);
        Ok(())
    }

    pub fn install_current_as_idle(&mut self, cpu: CpuIndex) -> Result<(usize, usize), Error> {
        let current = self.current_thread(cpu)?;
        if let Some(owner) = self.cpu_lock_required_for(current)? {
            return self.with_cpu_schedule_stored(owner, |scheduler| {
                scheduler.install_current_as_idle(cpu)
            });
        }
        let cpu_slot = self.cpu_slot(cpu)?;
        if self.idle_thread(cpu_slot)?.is_some() {
            return Err(Error::IdleThreadAlreadyInstalled);
        }
        let current = self.current_thread(cpu_slot)?;
        if self.thread(current)?.state() != ThreadState::Running {
            return Err(Error::InvalidIdleTransition);
        }
        let mut thread = self.thread_mut(current)?;
        let stack = thread.ensure_kernel_stack()?;
        thread.become_idle();
        self.with_cpu_domain(cpu_slot, |_scheduler, local| {
            if local.idle.is_some() {
                return Err(Error::IdleThreadAlreadyInstalled);
            }
            local.idle = Some(current);
            Ok(())
        })?;
        self.schedulable_cpus = self.schedulable_cpus.with_cpu(cpu);
        Ok(stack)
    }

    /// Retires source-CPU context ownership from the incoming switch tail.
    pub fn complete_incoming_switch(
        &mut self,
        cpu: CpuIndex,
        ticket: u64,
    ) -> Result<CoordinatedTailCompletion, Error> {
        if self.active_domain.is_none() {
            return self.with_cpu_schedule_stored(cpu, |scheduler| {
                scheduler.complete_incoming_switch(cpu, ticket)
            });
        }
        let cpu_slot = self.cpu_slot(cpu)?;
        let switching = self.with_cpu_domain(cpu_slot, |_scheduler, local| {
            local
                .handoff
                .complete(ticket)
                .ok_or(Error::PreemptionInvariant)
        })?;
        let plan = self.thread_mut(switching.thread)?.take_migration_request();
        let state = self.thread(switching.thread)?.state();
        if !matches!(state, ThreadState::Ready | ThreadState::Idle)
            && (state != ThreadState::Blocked || plan.is_some())
        {
            let released = self
                .registry
                .with_thread_mut(switching.thread, |thread| thread.release_schedule(cpu))?;
            if !released {
                scheduler_invariant(Error::InvalidThreadState);
            }
        }
        let ready = match plan {
            Some(plan) => self.complete_migration(switching.thread, plan)?,
            None => None,
        };
        let retirement_published = self.queue_terminated_retirement(switching.thread)?;
        Ok(CoordinatedTailCompletion {
            ready,
            retirement_published,
        })
    }

    pub fn queue_terminated_retirement(&mut self, id: ThreadId) -> Result<bool, Error> {
        if self
            .registry
            .with_thread(id, Thread::schedule_owner_cpu)?
            .is_some()
        {
            return Ok(false);
        }
        let thread = self.thread(id)?;
        if thread.state() != ThreadState::Terminated {
            return Ok(false);
        }
        if !thread.wait_record().is_idle()
            || !self.with_thread(id, |thread| thread.wait_context().is_idle())?
        {
            return Err(Error::InvalidWaitRegistration);
        }
        if thread.queue_links().membership != QueueMembership::Terminated {
            return Err(Error::QueueCorrupted);
        }
        Self::queue_remove(
            &mut self.registry,
            &mut self.terminated,
            id,
            QueueMembership::Terminated,
        )?;
        self.registry.begin_retirement(id)?;
        if let Some(tail) = self.retirement_tail {
            if self.registry.link_retirement(tail, id).is_err() {
                scheduler_invariant(Error::QueueCorrupted);
            }
        } else {
            self.retirement_head = Some(id);
        }
        self.retirement_tail = Some(id);
        Ok(true)
    }

    pub fn take_retirement(&mut self) -> Result<Option<RetiredThread>, Error> {
        let Some(id) = self.retirement_head else {
            return Ok(None);
        };
        let (retired, next) = self.registry.take_retiring(id)?;
        self.retirement_head = next;
        if next.is_none() {
            self.retirement_tail = None;
        }
        Ok(Some(retired))
    }

    pub fn complete_retirement(&mut self, id: ThreadId) -> Result<(), Error> {
        self.registry.complete_retirement(id)
    }

    pub const fn has_retirements(&self) -> bool {
        self.retirement_head.is_some()
    }

    pub fn statistics(&self) -> Statistics {
        let mut stats = Statistics::default();
        self.registry.for_each_thread(|thread| {
            stats.threads += 1;
            if thread.schedule_owner_cpu().is_some() {
                return;
            }
            match thread.scheduling_class() {
                SchedulingClass::RealTime => stats.real_time_class_threads += 1,
                SchedulingClass::Fair => stats.fair_class_threads += 1,
                SchedulingClass::Budget => stats.budget_class_threads += 1,
                SchedulingClass::Idle => stats.idle_class_threads += 1,
            }
            match thread.state() {
                ThreadState::Ready => stats.ready += 1,
                ThreadState::Running => stats.running += 1,
                ThreadState::Blocked => stats.blocked += 1,
                ThreadState::Migrating => stats.migrating += 1,
                ThreadState::Idle => stats.idle += 1,
                ThreadState::Dormant | ThreadState::Terminated => {}
            }
        });
        for index in 0..hyper::cpu::MAX_CPUS {
            let Some(cpu) = CpuIndex::new(index) else {
                hyper::debug::invariant_failure("task::scheduler::state::statistics invariant");
            };
            CPU_SCHEDULERS[cpu].with(|slot| {
                let Some(local) = slot.as_mut() else {
                    return;
                };
                stats.context_switches =
                    stats.context_switches.saturating_add(local.handoff.count());
                let topology_ready = local.run_queue.len();
                let topology_real_time_ready = local.run_queue.real_time_len();
                let topology_fair_ready = local.run_queue.fair_len();
                let current = local.current;
                stats.per_cpu_ready[index] = topology_ready;
                let mut observed_ready = 0usize;
                let mut observed_real_time_ready = 0usize;
                let mut observed_fair_ready = 0usize;
                let threads = local.thread_authority();
                self.registry.for_each_thread(|thread| {
                    if thread.schedule_owner_cpu() != Some(cpu) {
                        return;
                    }
                    if threads
                        .with_thread(thread.id(), |_thread, schedule| {
                            let class = schedule.scheduling_class();
                            match class {
                                SchedulingClass::RealTime => stats.real_time_class_threads += 1,
                                SchedulingClass::Fair => stats.fair_class_threads += 1,
                                SchedulingClass::Budget => stats.budget_class_threads += 1,
                                SchedulingClass::Idle => stats.idle_class_threads += 1,
                            }
                            match schedule.state {
                                ThreadState::Ready => {
                                    stats.ready += 1;
                                    observed_ready += 1;
                                    match class {
                                        SchedulingClass::RealTime => observed_real_time_ready += 1,
                                        SchedulingClass::Fair | SchedulingClass::Budget => {
                                            observed_fair_ready += 1
                                        }
                                        SchedulingClass::Idle => hyper::debug::invariant_failure(
                                            "task::scheduler::state::statistics invariant",
                                        ),
                                    }
                                }
                                ThreadState::Running => stats.running += 1,
                                ThreadState::Blocked => stats.blocked += 1,
                                ThreadState::Migrating => stats.migrating += 1,
                                ThreadState::Idle => stats.idle += 1,
                                ThreadState::Dormant | ThreadState::Terminated => {}
                            }
                        })
                        .is_err()
                    {
                        hyper::debug::invariant_failure(
                            "task::scheduler::state::statistics invariant",
                        );
                    }
                });
                if observed_ready != topology_ready
                    || observed_real_time_ready != topology_real_time_ready
                    || observed_fair_ready != topology_fair_ready
                {
                    hyper::debug::invariant_failure("task::scheduler::state::statistics invariant");
                }
                if threads.with_thread(current, |_thread, schedule| {
                    matches!(schedule.state, ThreadState::Running | ThreadState::Idle)
                }) != Ok(true)
                {
                    hyper::debug::invariant_failure("task::scheduler::state::statistics invariant");
                }
            });
        }
        stats
    }

    #[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
    pub const fn registry_slot_count(&self) -> usize {
        self.registry.high_water()
    }

    pub fn with_thread<R>(
        &self,
        id: ThreadId,
        operation: impl for<'thread> FnOnce(&'thread Thread) -> R,
    ) -> Result<R, Error> {
        self.registry.with_thread(id, operation)
    }

    pub(super) fn thread(&self, id: ThreadId) -> Result<ThreadObservation, Error> {
        self.registry.with_thread(id, |thread| {
            match thread.schedule_owner_cpu() {
                None => ThreadObservation::capture(thread),
                Some(cpu) if self.active_domain.is_some_and(|active| active.cpu == cpu) => {
                    // SAFETY: `active_domain` is installed only while
                    // `with_cpu_schedule_stored` holds this exact CPU lock.
                    unsafe {
                        thread.with_cpu_schedule(cpu, |schedule| {
                            ThreadObservation::capture_cpu(thread, schedule)
                        })
                    }
                    .unwrap_or_else(|| {
                        hyper::debug::invariant_failure("task::scheduler::state::thread invariant")
                    })
                }
                Some(_) => ThreadObservation {
                    schedule: None,
                    execution: thread.execution_kind(),
                    context: thread.context_pointer(),
                },
            }
        })
    }

    fn thread_mut(&mut self, id: ThreadId) -> Result<ThreadMutation<'_>, Error> {
        let owner = self.registry.with_thread(id, Thread::schedule_owner_cpu)?;
        let cpu = match (owner, self.active_domain) {
            (None, _) => None,
            (Some(cpu), Some(active)) if cpu == active.cpu => Some(cpu),
            (Some(_), _) => return Err(Error::InvalidThreadState),
        };
        Ok(ThreadMutation {
            registry: &mut self.registry,
            id,
            cpu,
        })
    }

    /// Runs one coordinator transaction while holding the matching CPU lock.
    ///
    /// Schedule storage remains in its stable Thread cell and its owner
    /// locator remains `Cpu(cpu)`: ready-to-current transitions therefore do
    /// not transfer or recreate schedule state. `active_domain` is private proof
    /// used only to route closure-bounded Thread access after revalidation.
    fn with_cpu_schedule_stored<R>(
        &mut self,
        cpu: CpuIndex,
        operation: impl FnOnce(&mut Self) -> Result<R, Error>,
    ) -> Result<R, Error> {
        let result = self.with_cpu_domain(cpu, |scheduler, local| {
            let current = local.current;
            let current_owner = scheduler
                .registry
                .with_thread(current, Thread::schedule_owner_cpu);
            if current_owner != Ok(Some(cpu)) {
                hyper::debug::invariant_failure(
                    "task::scheduler::state::with_cpu_schedule_stored invariant",
                );
            }
            operation(scheduler)
        });
        if let Some((id, target)) = self.deferred_ready_handoff.take() {
            if self.active_domain.is_some() {
                hyper::debug::invariant_failure(
                    "task::scheduler::state::with_cpu_schedule_stored invariant",
                );
            }
            if let Err(error) = self.enqueue_ready(id) {
                scheduler_invariant(error);
            }
            if self.registry.with_thread(id, Thread::schedule_owner_cpu) != Ok(Some(target)) {
                hyper::debug::invariant_failure(
                    "task::scheduler::state::with_cpu_schedule_stored invariant",
                );
            }
        }
        if let Some((id, target)) = self.deferred_blocked_handoff.take()
            && let Err(error) = self.publish_blocked_handoff(id, target)
        {
            scheduler_invariant(error);
        }
        result
    }

    fn with_cpu_domain<R>(
        &mut self,
        cpu: CpuIndex,
        operation: impl FnOnce(&mut Self, &mut CpuScheduler) -> Result<R, Error>,
    ) -> Result<R, Error> {
        if let Some(active) = self.active_domain {
            if active.cpu != cpu {
                hyper::debug::invariant_failure(
                    "task::scheduler::state::with_cpu_domain invariant",
                );
            }
            // SAFETY: the matching CPU lock remains held by the outer scope;
            // nested access is serialized by the exclusive Scheduler borrow.
            return operation(self, unsafe { &mut *active.local.as_ptr() });
        }
        CPU_SCHEDULERS[cpu].with(|slot| {
            let local = match slot.as_mut() {
                Some(local) => local,
                None => return Err(Error::CpuNotRegistered),
            };
            if local.index != cpu {
                hyper::debug::invariant_failure(
                    "task::scheduler::state::with_cpu_domain invariant",
                );
            }
            local.account_current()?;
            let local = core::ptr::NonNull::from(&mut *local);
            self.active_domain = Some(ActiveCpuDomain { cpu, local });
            // SAFETY: this is the first and only reborrow after publishing
            // `local`. Nested accesses derive from the same pointer, and the
            // original lock-slot borrow is not used again. Kernel profiles
            // use panic=abort, so unwinding cannot strand active_domain.
            let result = operation(self, unsafe { &mut *local.as_ptr() });
            self.active_domain = None;
            // SAFETY: the same CPU lock still protects this pointer.
            unsafe { &mut *local.as_ptr() }.refresh_deadline()?;
            result
        })
    }

    fn prepare_switch(
        &mut self,
        cpu_slot: CpuIndex,
        current: ThreadId,
        next: ThreadId,
        _reason: SwitchReason,
        _point: SchedulePoint,
    ) -> Result<PreparedContextSwitch, Error> {
        let target_cpu = cpu_slot;
        if self.switching_from(cpu_slot)?.is_some() {
            return Err(Error::ThreadTransitionInProgress);
        }
        if self.thread(next)?.state() == ThreadState::Ready
            && !self.thread_mut(next)?.mark_running_on(target_cpu)
        {
            return Err(Error::InvalidThreadState);
        }
        if self.thread(next)?.fair_slice_expired() {
            self.thread_mut(next)?.replenish_fair_slice();
        }
        if self.thread(next)?.state() == ThreadState::Idle {
            super::balance::request();
        }
        let generation = self.with_cpu_domain(cpu_slot, |_scheduler, local| {
            if local.handoff.current().is_some() || local.current != current {
                return Err(Error::ThreadTransitionInProgress);
            }
            let generation = local
                .handoff
                .begin(current, SwitchDisposition::Coordinated)
                .unwrap_or_else(|| {
                    hyper::debug::invariant_failure(
                        "task::scheduler::state::prepare_switch invariant",
                    )
                });
            local.current = next;
            Ok(generation)
        })?;
        crate::tracepoint!(
            Scheduler,
            "sched_switch",
            prev = current.get(),
            next = next.get(),
            state = super::trace::switch_state(
                self.thread(current)
                    .map_or(0, |thread| thread.state() as u64),
                self.thread(next)
                    .map_or(0, |thread| thread.scheduling_class() as u64)
            ),
            cause = super::trace::switch_cause(_reason, _point)
        );
        let previous = self.thread(current)?.context_pointer();
        let next = self.thread(next)?.context_pointer().cast_const();
        Ok(PreparedContextSwitch {
            previous,
            next,
            ticket: generation,
            armed: true,
        })
    }

    fn enqueue_ready(&mut self, id: ThreadId) -> Result<(), Error> {
        let thread = self.thread(id)?;
        let cpu = thread.cpu_index();
        if thread.scheduling_class() == SchedulingClass::Idle || !thread.can_run_on(cpu) {
            return Err(Error::InvalidThreadState);
        }
        self.with_cpu_domain(cpu, |scheduler, local| {
            let floor = local.fair_floor;
            scheduler.thread_mut(id)?.apply_schedule(|schedule| {
                runtime::prepare_enqueue(schedule, floor, false, local.now())
            });
            let cpu_threads = local.table.cpu_authority(local.index, &mut local.authority);
            let coordinator = scheduler.registry.write_authority();
            let mut threads = queue::ReadyQueueAuthority::new(coordinator, cpu_threads);
            local.run_queue.enqueue(&mut threads, id, cpu)
        })
    }

    fn enqueue_ready_or_defer(&mut self, id: ThreadId, target: CpuIndex) -> Result<(), Error> {
        if self
            .active_domain
            .is_some_and(|active| active.cpu != target)
        {
            if self.deferred_ready_handoff.replace((id, target)).is_some() {
                hyper::debug::invariant_failure(
                    "task::scheduler::state::enqueue_ready_or_defer invariant",
                );
            }
            // The changed ReadyOutcome prompts the target after this handoff.
            return Ok(());
        }
        self.enqueue_ready(id)
    }

    fn enqueue_ready_front(&mut self, id: ThreadId) -> Result<(), Error> {
        let thread = self.thread(id)?;
        let cpu = thread.cpu_index();
        if thread.scheduling_class() == SchedulingClass::Idle || !thread.can_run_on(cpu) {
            return Err(Error::InvalidThreadState);
        }
        self.with_cpu_domain(cpu, |scheduler, local| {
            let floor = local.fair_floor;
            scheduler.thread_mut(id)?.apply_schedule(|schedule| {
                runtime::prepare_enqueue(schedule, floor, true, local.now())
            });
            let cpu_threads = local.table.cpu_authority(local.index, &mut local.authority);
            let coordinator = scheduler.registry.write_authority();
            let mut threads = queue::ReadyQueueAuthority::new(coordinator, cpu_threads);
            local.run_queue.enqueue_front(&mut threads, id, cpu)
        })
    }

    /// Turns a latency-sensitive equal-Fair wakeup into a scheduling boundary.
    ///
    /// This is deliberately narrower than priority preemption: it neither
    /// displaces real-time work nor changes the woken entity's queue position.
    fn expire_current_for_fair_wakeup(&mut self, id: ThreadId) -> Result<(), Error> {
        let cpu = self
            .registry
            .with_thread(id, Thread::schedule_owner_cpu)?
            .ok_or(Error::InvalidThreadState)?;
        self.with_cpu_domain(cpu, |_scheduler, local| {
            let candidate_is_fair = {
                let threads = local.thread_authority();
                threads.with_thread(id, |_thread, schedule| {
                    schedule.scheduling_class() == SchedulingClass::Fair
                })?
            };
            if !candidate_is_fair {
                return Ok(());
            }
            let current = local.current;
            let mut threads = local.thread_authority();
            threads.with_thread_mut(current, |_thread, schedule| {
                if schedule.state != ThreadState::Running
                    || schedule.scheduling_class() != SchedulingClass::Fair
                {
                    return;
                }
                schedule.expire_fair_slice();
            })
        })
    }

    fn dequeue_ready(&mut self, cpu: CpuIndex) -> Result<Option<ThreadId>, Error> {
        self.with_cpu_domain(cpu, |_scheduler, local| local.local_dequeue_ready())
    }

    fn remove_ready(
        &mut self,
        id: ThreadId,
        cpu: CpuIndex,
        membership: QueueMembership,
    ) -> Result<(), Error> {
        self.with_cpu_domain(cpu, |scheduler, local| {
            let cpu_threads = local.table.cpu_authority(local.index, &mut local.authority);
            let coordinator = scheduler.registry.write_authority();
            let mut threads = queue::ReadyQueueAuthority::new(coordinator, cpu_threads);
            local.run_queue.remove(&mut threads, id, cpu, membership)
        })
    }

    fn remove_waiter(&mut self, wait_queue: &WaitQueue, id: ThreadId) -> Result<(), Error> {
        let membership = QueueMembership::Waiting {
            queue: wait_queue.identity(),
        };
        // SAFETY: Every caller holds the global scheduler lock exclusively.
        Self::queue_remove(
            &mut self.registry,
            unsafe { &mut *wait_queue.state_pointer() },
            id,
            membership,
        )
    }

    fn cpu_slot(&self, cpu: CpuIndex) -> Result<CpuIndex, Error> {
        if self.active_domain.is_some_and(|active| active.cpu == cpu)
            || CPU_SCHEDULERS[cpu].with(|slot| slot.is_some())
        {
            Ok(cpu)
        } else {
            Err(Error::CpuNotRegistered)
        }
    }

    fn schedulable_cpu_slot(&self, cpu: CpuIndex) -> Result<CpuIndex, Error> {
        if self.cpu_is_schedulable(cpu) {
            Ok(cpu)
        } else {
            Err(Error::CpuNotRegistered)
        }
    }

    fn cpu_is_schedulable(&self, cpu: CpuIndex) -> bool {
        self.schedulable_cpus.contains(cpu)
    }

    fn idle_thread(&self, cpu: CpuIndex) -> Result<Option<ThreadId>, Error> {
        if let Some(active) = self.active_domain
            && active.cpu == cpu
        {
            // SAFETY: active domain is bounded by the matching lock closure.
            return Ok(unsafe { active.local.as_ref() }.idle);
        }
        CPU_SCHEDULERS[cpu].with(|slot| {
            slot.as_ref()
                .map(|local| local.idle)
                .ok_or(Error::CpuNotRegistered)
        })
    }

    fn switching_from(&self, cpu: CpuIndex) -> Result<Option<SwitchingContext<ThreadId>>, Error> {
        if let Some(active) = self.active_domain
            && active.cpu == cpu
        {
            // SAFETY: active domain is bounded by the matching lock closure.
            return Ok(unsafe { active.local.as_ref() }.handoff.current());
        }
        CPU_SCHEDULERS[cpu].with(|slot| {
            slot.as_ref()
                .map(|local| local.handoff.current())
                .ok_or(Error::CpuNotRegistered)
        })
    }

    fn peek_ready(&mut self, cpu: CpuIndex) -> Result<Option<queue::ReadyThread>, Error> {
        self.with_cpu_domain(cpu, |_scheduler, local| local.local_peek_ready())
    }

    fn queue_push(
        registry: &mut ThreadRegistry,
        target: &mut ThreadQueue,
        id: ThreadId,
        membership: QueueMembership,
    ) -> Result<(), Error> {
        let mut threads = queue::ControlQueueAuthority::new(registry.control_authority());
        queue::control_push(&mut threads, target, id, membership)
    }

    fn queue_remove(
        registry: &mut ThreadRegistry,
        target: &mut ThreadQueue,
        id: ThreadId,
        membership: QueueMembership,
    ) -> Result<(), Error> {
        let mut threads = queue::ControlQueueAuthority::new(registry.control_authority());
        queue::control_remove(&mut threads, target, id, membership)
    }

    pub fn thread_table_growth(&self) -> Option<usize> {
        self.registry.growth_needed()
    }

    pub fn install_thread_table_growth(&mut self, growth: &mut super::registry::TableGrowth) {
        self.registry.install_growth(growth);
    }

    fn reserve_thread_slot(&mut self, cpu: CpuIndex) -> Result<ThreadReservation, Error> {
        self.registry.reserve(cpu)
    }
}

impl From<WaitRecordError> for Error {
    fn from(error: WaitRecordError) -> Self {
        match error {
            WaitRecordError::GenerationExhausted => Self::WaitGenerationExhausted,
            WaitRecordError::RegistrationMismatch | WaitRecordError::InvalidPhase => {
                Self::InvalidWaitRegistration
            }
        }
    }
}

fn scheduler_invariant(_error: Error) -> ! {
    // The caller holds CPU/queue locks or exclusive coordination after a
    // committed mutation. Ordinary logs could deadlock and returning would
    // expose inconsistent state. The panic path retains all protection.
    hyper::debug::invariant_failure("task::scheduler::state::scheduler_invariant invariant")
}
