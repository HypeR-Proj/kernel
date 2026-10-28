// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Process composition, publication, stop, and explicit retirement.
//!
//! Handle-table operations and their linear transactions live in private
//! children; this owner retains Process state, Thread membership, and teardown.

mod handle_accounting;
mod handle_transactions;
mod handles;

use handle_accounting::HandleAccounting;
use handles::{HandleAdmission, require_handle_phase};
mod start;

pub(crate) use handle_transactions::{
    PreparedDirectProcessHandleTransfer, ProcessHandleBatchReservation, ProcessHandleReservation,
};
pub(crate) use start::{ChildProcessStartError, ProcessStartCoordinator, StartedChildProcess};

use super::builder::{ProcessStartTransaction, SealedProcessBuild, StartPreparationFailure};
use super::directory::PreparedRegistration;
use super::image::{MachineAbi, ProcessImage, UserThreadStart};
use super::lifecycle::{
    LifecycleError, ProcessLifecycle, ProcessPhase, StopDispatchProgress, TerminalReason,
};
use super::objects::ProcessObject;
use super::task_group::{
    PreparedTaskGroupMembership, TaskGroup, TaskGroupError, TaskGroupMembership,
};
use super::user_thread::{UserExecution, UserExecutionOwnership, UserThread};
use crate::sync::Completion;
use crate::task::thread::ThreadId;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use hyper::mm::{FallibleArc, UniqueFallibleArc, WeakFallibleArc};
use hyper::sync::{InterruptSpinLock, PublishedOnce};
use {
    crate::accounting::ChargeReservation, crate::accounting::CommittedCharge,
    crate::accounting::ResourceAmount, crate::accounting::ResourceDomain,
    crate::accounting::ResourceError, crate::accounting::ResourceKind,
};
use {
    crate::capability::HandleBatchReservationStorage, crate::capability::HandleError,
    crate::capability::HandleFlags, crate::capability::HandleSidecar,
    crate::capability::HandleSidecarPlan, crate::capability::HandleTable,
    crate::capability::HandleValue, crate::capability::InTransitCapabilities,
    crate::capability::PreparedHandle, crate::capability::RetiredHandleBatchReservationStorage,
    crate::capability::Rights,
};
use {
    crate::mm::user_space::MachineError, crate::mm::user_space::MemoryObjectError,
    crate::mm::user_space::NativeAddressSpace, crate::mm::user_space::NativeUserRead,
    crate::mm::user_space::UserSlice, crate::mm::user_space::UserWriteReservation,
    crate::mm::user_space::VmarObject,
};
use {
    crate::object::KernelService, crate::object::Koid, crate::object::ObjectCreationError,
    crate::object::ObjectPublication, crate::object::PublishableRef, crate::object::SignalMask,
};
use {crate::task::scheduler, crate::task::scheduler::CpuMask};

type ProcessLock<T> = InterruptSpinLock<T, crate::hal::irq::LocalMask>;

static NEXT_PROCESS_ID: AtomicU64 = AtomicU64::new(1);

struct RetirementQueue {
    ready: RetirementList,
    delayed: RetirementList,
}

#[derive(Clone)]
struct RetirementLink(Process);

struct RetirementList {
    head: Option<RetirementLink>,
    tail: Option<RetirementLink>,
}

static RETIREMENTS: ProcessLock<RetirementQueue> = ProcessLock::new(RetirementQueue {
    ready: RetirementList {
        head: None,
        tail: None,
    },
    delayed: RetirementList {
        head: None,
        tail: None,
    },
});

fn queue_retirement(process: Process) {
    if process.inner.retirement_queued.swap(true, Ordering::AcqRel) {
        return;
    }
    RETIREMENTS.with(|queue| queue.ready.push_back(process));
}

#[derive(Clone, Copy)]
pub(crate) struct RetirementWork {
    pub(crate) ready: bool,
    pub(crate) delayed: bool,
}

/// Reports immediately runnable and timer-delayed Process retirement work.
pub(crate) fn retirement_work(_access: &crate::reaper::ReaperAccess) -> RetirementWork {
    RETIREMENTS.with(|queue| RetirementWork {
        ready: !queue.ready.is_empty(),
        delayed: !queue.delayed.is_empty(),
    })
}

/// Makes one delayed retry round visible after the retry timer expires.
pub(crate) fn promote_delayed_retirements(_access: &mut crate::reaper::ReaperAccess) {
    RETIREMENTS.with(|queue| {
        // Expired retries precede newer arrivals. Otherwise a sustained stream
        // of stopped Processes could keep an older retained owner behind an
        // ever-growing ready tail even though its retry deadline has passed.
        queue.delayed.append(&mut queue.ready);
        core::mem::swap(&mut queue.ready, &mut queue.delayed);
    });
}

/// Performs one Process step per reaper batch, alternating with object/thread
/// destruction so the references being awaited can themselves be released.
pub(crate) fn reap_one_process(_access: &mut crate::reaper::ReaperAccess) {
    let process = RETIREMENTS.with(|queue| queue.ready.pop_front());
    let Some(process) = process else {
        return;
    };
    let retry = process.inner.retirement_retry.with(Option::take);
    let step = match retry {
        Some(retry) => match retry.retry() {
            Ok(())
                if process.inner.state.with(|state| state.lifecycle.phase())
                    == ProcessPhase::Retired =>
            {
                ProcessRetirementStep::Complete
            }
            Ok(()) => ProcessRetirementStep::InProgress,
            Err((retry, _)) => ProcessRetirementStep::Retry(retry),
        },
        None => match process.retire() {
            Ok(step) => step,
            Err(ProcessError::Handle(HandleError::OutstandingReservation)) => {
                ProcessRetirementStep::InProgress
            }
            Err(error) => crate::diagnostics::crash::fatal(format_args!(
                "HypeR: Process retirement failed: {error:?}"
            )),
        },
    };
    match step {
        ProcessRetirementStep::Complete => {}
        ProcessRetirementStep::Retry(retry) => {
            process
                .inner
                .retirement_retry
                .with(|slot| *slot = Some(retry));
            RETIREMENTS.with(|queue| queue.delayed.push_back(process));
        }
        ProcessRetirementStep::InProgress | ProcessRetirementStep::PendingReferences => {
            RETIREMENTS.with(|queue| queue.delayed.push_back(process));
        }
    }
}

impl hyper::collections::linked_list::TailLink for RetirementLink {
    fn link_successor(&self, next: Self) {
        self.0
            .inner
            .retirement_next
            .with(|link| *link = Some(next.0));
    }
    fn take_successor(&self) -> Option<Self> {
        self.0.inner.retirement_next.with(Option::take).map(Self)
    }
}
impl RetirementList {
    fn is_empty(&self) -> bool {
        self.head.is_none()
    }
    fn push_back(&mut self, process: Process) {
        hyper::collections::linked_list::push_back(
            &mut self.head,
            &mut self.tail,
            RetirementLink(process),
        );
    }
    fn pop_front(&mut self) -> Option<Process> {
        hyper::collections::linked_list::pop_front_with_tail(&mut self.head, &mut self.tail)
            .map(|link| link.0)
    }
    fn append(&mut self, other: &mut Self) {
        hyper::collections::linked_list::append(
            &mut self.head,
            &mut self.tail,
            &mut other.head,
            &mut other.tail,
        );
    }
}

fn machine_matches_host(machine: MachineAbi) -> bool {
    let requested = match machine {
        MachineAbi::Aarch64 => crate::hal::user::HostMachine::Aarch64,
        MachineAbi::Riscv64 => crate::hal::user::HostMachine::Riscv64,
        MachineAbi::X86_64 => crate::hal::user::HostMachine::X86_64,
    };
    requested == crate::hal::user::host_machine()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ProcessId(u64);

impl ProcessId {
    pub(crate) const fn get(self) -> u64 {
        self.0
    }
}

struct ThreadRecord {
    active: AtomicBool,
    scheduler_id: AtomicU64,
    next: ProcessLock<Option<FallibleArc<ThreadRecord>>>,
    _metadata_charge: CommittedCharge,
}

struct ProcessState {
    lifecycle: ProcessLifecycle,
    address_space: Option<FallibleArc<NativeAddressSpace>>,
    group_membership: Option<TaskGroupMembership>,
    process_charge: Option<CommittedCharge>,
    threads: Option<FallibleArc<ThreadRecord>>,
    handle_accounting: HandleAccounting,
    // Backing quota follows page reclamation; retained directory/generation
    // storage stays charged until final table retirement.
    handle_table_charge: Option<CommittedCharge>,
    handles_retired: bool,
    stopping_handle_cursor: usize,
    address_retired: bool,
}

struct HandleChargeState {
    entries: alloc::vec::Vec<HandleChargeEntry>,
}

struct HandleChargeEntry {
    value: HandleValue,
    charge: Option<CommittedCharge>,
}

#[derive(Clone)]
struct HandleChargeLocation {
    record: FallibleArc<HandleChargeRecord>,
    entry: usize,
}

struct HandleChargeRecord {
    previous: ProcessLock<Option<WeakFallibleArc<HandleChargeRecord>>>,
    state: ProcessLock<HandleChargeState>,
    next: ProcessLock<Option<FallibleArc<HandleChargeRecord>>>,
    _metadata_charge: CommittedCharge,
}

pub(super) struct ProcessInner {
    // Declared first so directory metadata is detached before payload charges
    // are released by field destruction.
    pub(super) directory: super::directory::Membership,
    id: ProcessId,
    group_id: super::TaskGroupId,
    domain_id: crate::accounting::ResourceDomainId,
    image_generation: u64,
    pub(super) scheduling: PublishedOnce<super::scheduling::ProcessScheduling>,
    name: PublishedOnce<ProcessNameSnapshot>,
    image: ProcessImage,
    domain: ResourceDomain,
    handles: ProcessLock<HandleTable>,
    state: ProcessLock<ProcessState>,
    object: PublishedOnce<PublishableRef<ProcessObject, KernelService>>,
    stopped: Completion,
    stop_requested: AtomicBool,
    retirement_queued: AtomicBool,
    retention: PublishedOnce<FallibleArc<super::retention::Retention>>,
    retention_budget: ProcessLock<Option<FallibleArc<super::retention::SupervisorBudget>>>,
    retirement_next: ProcessLock<Option<Process>>,
    retirement_retry: ProcessLock<Option<AddressSpaceRetirement>>,
    _metadata_charge: CommittedCharge,
}

#[derive(Debug)]
pub(crate) enum ProcessError {
    Allocation,
    Handle(HandleError),
    Object(ObjectCreationError),
    Lifecycle(
        #[expect(dead_code, reason = "Retains the cause for derived Debug diagnostics")]
        LifecycleError,
    ),
    UserMemory(MachineError),
    Resource(ResourceError),
    Scheduler(scheduler::Error),
    TaskGroup(TaskGroupError),
    AddressSpaceReferenced,
    UserEntry(crate::hal::user::UserEntryError),
}

impl From<HandleError> for ProcessError {
    fn from(error: HandleError) -> Self {
        Self::Handle(error)
    }
}

impl From<ObjectCreationError> for ProcessError {
    fn from(error: ObjectCreationError) -> Self {
        Self::Object(error)
    }
}

impl From<LifecycleError> for ProcessError {
    fn from(error: LifecycleError) -> Self {
        Self::Lifecycle(error)
    }
}

impl From<MachineError> for ProcessError {
    fn from(error: MachineError) -> Self {
        Self::UserMemory(error)
    }
}

impl From<ResourceError> for ProcessError {
    fn from(error: ResourceError) -> Self {
        Self::Resource(error)
    }
}

impl From<scheduler::Error> for ProcessError {
    fn from(error: scheduler::Error) -> Self {
        Self::Scheduler(error)
    }
}

impl From<TaskGroupError> for ProcessError {
    fn from(error: TaskGroupError) -> Self {
        Self::TaskGroup(error)
    }
}

/// Creation failure which preserves the unpublished machine address space.
#[must_use = "recover the unpublished address space with into_parts"]
pub(crate) struct ProcessCreateFailure {
    error: Option<ProcessError>,
    address_space: Option<UniqueFallibleArc<NativeAddressSpace>>,
}

impl ProcessCreateFailure {
    /// Recovers both the error and the still-linear machine owner.
    pub(crate) fn into_parts(mut self) -> (ProcessError, UniqueFallibleArc<NativeAddressSpace>) {
        let error = match self.error.take() {
            Some(error) => error,
            None => process_invariant_violation(),
        };
        let address_space = match self.address_space.take() {
            Some(address_space) => address_space,
            None => process_invariant_violation(),
        };
        (error, address_space)
    }
}

impl Drop for ProcessCreateFailure {
    fn drop(&mut self) {
        if self.error.is_some() || self.address_space.is_some() {
            // NativeAddressSpace deliberately cannot retire from Drop. Force
            // callers to recover the linear owner instead of silently leaking
            // its published translation identifier and machine root.
            process_invariant_violation();
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ProcessSnapshot {
    pub(crate) id: ProcessId,
    pub(crate) koid: Koid,
    pub(crate) group_id: super::TaskGroupId,
    pub(crate) domain_id: crate::accounting::ResourceDomainId,
    pub(crate) image_generation: u64,
    pub(crate) name: ProcessNameSnapshot,
    pub(crate) phase: ProcessPhase,
    pub(crate) pending_threads: usize,
    pub(crate) active_threads: usize,
    pub(crate) terminal: Option<TerminalReason>,
}

/// Immutable, allocation-free Process name retained through final observation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ProcessNameSnapshot {
    bytes: [u8; Self::CAPACITY],
    len: u8,
}

impl ProcessNameSnapshot {
    pub(crate) const CAPACITY: usize = 64;

    pub(crate) fn from_validated(name: &str) -> Self {
        if name.len() > Self::CAPACITY {
            process_invariant_violation();
        }
        let mut bytes = [0; Self::CAPACITY];
        bytes[..name.len()].copy_from_slice(name.as_bytes());
        Self {
            bytes,
            len: name.len() as u8,
        }
    }

    pub(crate) fn as_bytes(&self) -> &[u8] {
        &self.bytes[..usize::from(self.len)]
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ProcessStopReport {
    pub(crate) newly_requested: bool,
    pub(crate) dispatched_threads: usize,
    pub(crate) dispatch_complete: bool,
}

pub(crate) enum ProcessRetirementStep {
    Complete,
    InProgress,
    PendingReferences,
    Retry(AddressSpaceRetirement),
}

/// Strong process owner. Active `TaskGroup` membership pins one owner cycle until
/// explicit Process retirement removes it.
pub(crate) struct Process {
    pub(super) inner: FallibleArc<ProcessInner>,
}

#[must_use = "publish or abort the prepared process"]
pub(crate) struct PreparedProcess {
    process: Option<Process>,
    group: Option<PreparedTaskGroupMembership>,
    registration: Option<PreparedRegistration>,
}

impl PreparedProcess {
    pub(crate) fn try_new(
        image: ProcessImage,
        group: TaskGroup,
        domain: ResourceDomain,
        address_space: UniqueFallibleArc<NativeAddressSpace>,
    ) -> Result<Self, ProcessCreateFailure> {
        let group_membership = match group.prepare_membership() {
            Ok(membership) => membership,
            Err(error) => return Err(create_failure(error.into(), address_space)),
        };
        let metadata_amount = match process_metadata_amount() {
            Ok(amount) => amount,
            Err(error) => return Err(create_failure(error, address_space)),
        };
        let metadata_charge = match domain.reserve(metadata_amount) {
            Ok(charge) => charge.commit(),
            Err(error) => return Err(create_failure(error.into(), address_space)),
        };
        let process_amount = ResourceAmount::ZERO.with(ResourceKind::Processes, 1);
        let process_charge = match domain.reserve(process_amount) {
            Ok(charge) => charge.commit(),
            Err(error) => return Err(create_failure(error.into(), address_space)),
        };
        let inner_slot = match UniqueFallibleArc::try_new_uninit() {
            Ok(slot) => slot,
            Err(_) => return Err(create_failure(ProcessError::Allocation, address_space)),
        };
        let address_space = address_space.into_shared();
        let id = match allocate_process_id() {
            Ok(id) => id,
            Err(error) => {
                return Err(create_failure_from_arc(error, address_space));
            }
        };
        let handles = HandleTable::new(crate::random::next_u64());
        let handle_accounting = HandleAccounting::new(handles.new_sidecar());
        let inner = ProcessInner {
            directory: super::directory::Membership::new(id),
            id,
            group_id: group.id(),
            domain_id: domain.id(),
            image_generation: 1,
            scheduling: PublishedOnce::new(),
            name: PublishedOnce::new(),
            image,
            domain,
            handles: ProcessLock::new(handles),
            state: ProcessLock::new(ProcessState {
                lifecycle: ProcessLifecycle::prepared(),
                address_space: Some(address_space),
                group_membership: None,
                process_charge: Some(process_charge),
                threads: None,
                handle_accounting,
                handle_table_charge: None,
                handles_retired: false,
                stopping_handle_cursor: 0,
                address_retired: false,
            }),
            object: PublishedOnce::new(),
            stopped: Completion::new(),
            stop_requested: AtomicBool::new(false),
            retirement_queued: AtomicBool::new(false),
            retention: PublishedOnce::new(),
            retention_budget: ProcessLock::new(None),
            retirement_next: ProcessLock::new(None),
            retirement_retry: ProcessLock::new(None),
            _metadata_charge: metadata_charge,
        };
        // Storage was reserved while the address space still had its unique
        // rollback owner. Initialization cannot fail or return a whole
        // ProcessInner by value through an allocation-error branch.
        let inner = inner_slot.write(inner).into_shared();
        let process = Process { inner };
        let registration = match PreparedRegistration::try_new(&process) {
            Ok(registration) => registration,
            Err(error) => {
                let address_space = recover_unpublished_address_space(process);
                return Err(create_failure_from_arc(error, address_space));
            }
        };
        Ok(Self {
            process: Some(process),
            group: Some(group_membership),
            registration: Some(registration),
        })
    }

    pub(crate) fn process(&self) -> &Process {
        match self.process.as_ref() {
            Some(process) => process,
            None => process_invariant_violation(),
        }
    }

    fn reserve_startup_handle_batches(
        &self,
        count: usize,
    ) -> Result<alloc::vec::Vec<ProcessHandleBatchReservation>, ProcessError> {
        let maximum = HandleBatchReservationStorage::maximum_count();
        let batch_count = count
            .checked_add(maximum.saturating_sub(1))
            .map(|rounded| rounded / maximum)
            .ok_or(ProcessError::Allocation)?;
        let mut batches = alloc::vec::Vec::new();
        batches
            .try_reserve_exact(batch_count)
            .map_err(|_| ProcessError::Allocation)?;
        let mut remaining = count;
        while remaining != 0 {
            let batch_size = remaining.min(maximum);
            match self
                .process()
                .reserve_handle_batch_for(batch_size, HandleAdmission::PreparedChild)
            {
                Ok(batch) => batches.push(batch),
                Err(error) => {
                    for batch in batches.drain(..) {
                        self.process().abort_handle_batch(batch);
                    }
                    return Err(error);
                }
            }
            remaining -= batch_size;
        }
        Ok(batches)
    }

    fn address_space_owner(&self) -> FallibleArc<NativeAddressSpace> {
        self.process().inner.state.with(|state| {
            if state.lifecycle.phase() != ProcessPhase::Prepared {
                process_invariant_violation();
            }
            match state.address_space.as_ref() {
                Some(address_space) => address_space.clone(),
                None => process_invariant_violation(),
            }
        })
    }

    fn prepare_initial_user_thread(
        &self,
        name: &str,
        affinity: CpuMask,
        argument: u64,
    ) -> Result<PreparedInitialUserThread, ProcessError> {
        self.process()
            .prepare_initial_user_thread_unpublished(name, affinity, argument)
    }

    /// Publishes complete Process ownership and then makes it group-visible.
    pub(crate) fn publish(
        mut self,
        object: PublishableRef<ProcessObject, KernelService>,
        name: ProcessNameSnapshot,
    ) -> Process {
        let process = match self.process.take() {
            Some(process) => process,
            None => process_invariant_violation(),
        };
        if process.inner.name.publish(name).is_err() {
            process_invariant_violation();
        }
        process.install_object(object);
        if let Some(retention) = process.inner.retention.get() {
            retention.started(process.koid().get());
        }
        process.inner.state.with(|state| {
            if state.lifecycle.publish().is_err() {
                process_invariant_violation();
            }
        });
        let group = match self.group.take() {
            Some(group) => group,
            None => process_invariant_violation(),
        };
        let (membership, activation) = group.bind(process.clone());
        process.inner.state.with(|state| {
            if state.group_membership.replace(membership).is_some() {
                process_invariant_violation();
            }
        });
        match self.registration.take() {
            Some(registration) => registration.publish(&process),
            None => process_invariant_violation(),
        }
        // Directory publication must precede a pending stop. The stop path may
        // enqueue the Process for retirement on another CPU immediately; a
        // late directory publication would otherwise resurrect stale
        // discoverability after the Process was already retired.
        if let Some(generation) = activation.publish() {
            let _ = process.request_stop(TerminalReason::TaskGroupStop { generation });
        }
        process
    }

    /// Aborts unpublished Process construction and returns machine ownership.
    pub(crate) fn abort(mut self) -> UniqueFallibleArc<NativeAddressSpace> {
        let process = match self.process.take() {
            Some(process) => process,
            None => process_invariant_violation(),
        };
        if let Some(retention) = process.inner.retention.get() {
            retention.release(super::retention::RELEASE_ABORTED);
        }
        drop(self.group.take());
        drop(self.registration.take());
        let address = recover_unpublished_address_space(process);
        match address.try_into_unique() {
            Ok(address) => address,
            Err(_) => process_invariant_violation(),
        }
    }
}

impl Drop for PreparedProcess {
    fn drop(&mut self) {
        if self.process.is_some() || self.group.is_some() || self.registration.is_some() {
            process_invariant_violation();
        }
    }
}

impl Process {
    pub(crate) const TERMINATED: SignalMask =
        SignalMask::from_trusted_bits(hyper::abi::native::HYPER_NATIVE_SIGNAL_PROCESS_TERMINATED);
    pub(crate) const SUPPORTED_SIGNALS: SignalMask = Self::TERMINATED;

    pub(crate) fn id(&self) -> ProcessId {
        self.inner.id
    }

    pub(crate) fn name(&self) -> ProcessNameSnapshot {
        match self.inner.name.get() {
            Some(name) => *name,
            None => process_invariant_violation(),
        }
    }

    pub(crate) fn image(&self) -> &ProcessImage {
        &self.inner.image
    }

    pub(crate) fn image_generation(&self) -> u64 {
        self.inner.image_generation
    }

    pub(crate) fn resource_domain(&self) -> ResourceDomain {
        self.inner.domain.clone()
    }

    fn install_object(&self, object: PublishableRef<ProcessObject, KernelService>) {
        if self.inner.object.publish(object).is_err() {
            process_invariant_violation();
        }
    }

    pub(crate) fn koid(&self) -> Koid {
        match self.inner.object.get() {
            Some(object) => object.koid(),
            None => process_invariant_violation(),
        }
    }

    /// Retains the native address-space owner while Process handle admission
    /// remains open.
    ///
    /// Direct kernel references use this owner rather than manufacturing an
    /// internal handle. A userspace VMAR capability wraps the same owner and a
    /// generation-checked VMAR token.
    pub(crate) fn address_space_owner(
        &self,
    ) -> Result<FallibleArc<NativeAddressSpace>, ProcessError> {
        self.inner.state.with(|state| {
            require_handle_phase(state.lifecycle.phase())?;
            state
                .address_space
                .as_ref()
                .cloned()
                .ok_or(ProcessError::AddressSpaceReferenced)
        })
    }

    pub(crate) fn snapshot(&self) -> ProcessSnapshot {
        self.inner.state.with(|state| ProcessSnapshot {
            id: self.id(),
            koid: self.koid(),
            group_id: self.inner.group_id,
            domain_id: self.inner.domain_id,
            image_generation: self.image_generation(),
            name: match self.inner.name.get() {
                Some(name) => *name,
                None => process_invariant_violation(),
            },
            phase: state.lifecycle.phase(),
            pending_threads: state.lifecycle.pending_threads(),
            active_threads: state.lifecycle.active_threads(),
            terminal: state.lifecycle.terminal(),
        })
    }

    #[cfg_attr(
        all(feature = "kernel-test", feature = "kernel-self-test"),
        allow(
            dead_code,
            reason = "Native lifecycle self-tests require a HAL with user execution support"
        )
    )]
    pub(crate) fn start(&self) -> Result<(), ProcessError> {
        self.inner.state.with(|state| state.lifecycle.start())?;
        Ok(())
    }

    /// Starts and publishes readiness for the precommitted initial Thread.
    ///
    /// Holding Process state across scheduler readiness closes the valid race
    /// where a concurrent `TaskGroup` stop could terminate the dormant Thread
    /// after `Created -> Running` but before ready-queue publication.
    fn commit_initial_execution(&self, thread: ThreadId) {
        self.inner
            .state
            .with(|state| match state.lifecycle.phase() {
                ProcessPhase::Created => {
                    if state.lifecycle.start().is_err()
                        || scheduler::ready_user_thread(thread).is_err()
                    {
                        hyper::debug::invariant_failure(
                            "process::owner::commit_initial_execution invariant",
                        );
                    }
                }
                // A pending TaskGroup stop can reach retirement on another CPU
                // between Process publication and this final readiness step.
                // In every terminal phase the stop path already owns Thread
                // completion, so making the initial Thread ready is neither
                // necessary nor valid.
                ProcessPhase::Stopping
                | ProcessPhase::Stopped
                | ProcessPhase::Retiring
                | ProcessPhase::Retired => {}
                _ => process_invariant_violation(),
            });
    }

    #[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
    #[allow(
        dead_code,
        reason = "Join helpers are consumed by architecture-specific Native self-tests"
    )]
    pub(crate) fn join(&self) -> Result<TerminalReason, crate::sync::Error> {
        self.inner.stopped.wait()?;
        match self.snapshot().terminal {
            Some(reason) => Ok(reason),
            None => process_invariant_violation(),
        }
    }

    #[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
    #[allow(
        dead_code,
        reason = "Join helpers are consumed by architecture-specific Native self-tests"
    )]
    pub(crate) fn try_join(&self) -> Option<TerminalReason> {
        if !self.inner.stopped.try_wait() {
            return None;
        }
        match self.snapshot().terminal {
            Some(reason) => Some(reason),
            None => process_invariant_violation(),
        }
    }

    #[cfg_attr(
        all(feature = "kernel-test", feature = "kernel-self-test"),
        allow(
            dead_code,
            reason = "Used by the AArch64 Native self-tests; other HAL self-tests exercise different entry paths"
        )
    )]
    #[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
    pub(crate) fn create_initial_user_thread(
        &self,
        name: &str,
        affinity: CpuMask,
    ) -> Result<UserThread, ProcessError> {
        self.create_user_thread(name, self.image().initial_thread(), affinity)
    }

    pub(crate) fn create_user_thread(
        &self,
        name: &str,
        start: UserThreadStart,
        affinity: CpuMask,
    ) -> Result<UserThread, ProcessError> {
        self.create_user_thread_with_scheduling(name, start, affinity, self.scheduling().initial)
    }

    pub(crate) fn create_user_thread_with_scheduling(
        &self,
        name: &str,
        start: UserThreadStart,
        affinity: CpuMask,
        scheduling: crate::task::policy::SchedulingParameters,
    ) -> Result<UserThread, ProcessError> {
        if !machine_matches_host(self.image().machine()) {
            return Err(ProcessError::UserEntry(
                crate::hal::user::UserEntryError::Unsupported,
            ));
        }
        let prepared = self.prepare_user_thread()?;
        let execution =
            Self::prepare_user_execution(prepared.address_space.clone(), start, start.argument())?;
        let dormant = scheduler::prepare_user_thread_with_scheduling(
            name,
            prepared.thread.clone(),
            execution,
            crate::entry::user::thread_entry,
            affinity,
            scheduling,
        )?;
        let id = dormant.id();
        let thread = prepared.thread.clone();
        // Arming scheduler ownership before Process publication is safe because
        // the dormant ID has not escaped and cannot be made runnable. Every
        // subsequent operation is an infallible publication step.
        let terminal = prepared.publish(id, dormant);
        if let Some(reason) = terminal {
            let _ = scheduler::request_user_thread_stop(id, reason);
        }
        Ok(thread)
    }

    /// Holds the real creation rollback owner across a controlled test interleaving.
    #[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
    #[allow(
        dead_code,
        reason = "Used by Native lifecycle integration tests where user execution is available"
    )]
    pub(crate) fn prepare_thread_rollback_for_test(&self) -> Result<impl Drop + '_, ProcessError> {
        self.prepare_user_thread()
    }

    fn prepare_user_thread(&self) -> Result<PreparedUserThread, ProcessError> {
        self.inner
            .state
            .with(|state| state.lifecycle.reserve_thread())?;
        let prepared = self.prepare_user_thread_after_admission();
        if prepared.is_err() {
            self.abort_pending_thread();
        }
        prepared
    }

    fn prepare_initial_user_thread_unpublished(
        &self,
        name: &str,
        affinity: CpuMask,
        argument: u64,
    ) -> Result<PreparedInitialUserThread, ProcessError> {
        if !machine_matches_host(self.image().machine()) {
            return Err(ProcessError::UserEntry(
                crate::hal::user::UserEntryError::Unsupported,
            ));
        }
        self.inner
            .state
            .with(|state| state.lifecycle.reserve_initial_thread())?;
        let prepared = match self.prepare_user_thread_after_admission() {
            Ok(prepared) => prepared,
            Err(error) => {
                self.abort_pending_thread();
                return Err(error);
            }
        };
        let execution = Self::prepare_user_execution(
            prepared.address_space.clone(),
            self.image().initial_thread(),
            argument,
        )?;
        let dormant = scheduler::prepare_user_thread_with_scheduling(
            name,
            prepared.thread.clone(),
            execution,
            crate::entry::user::thread_entry,
            affinity,
            self.scheduling().initial,
        )?;
        Ok(PreparedInitialUserThread {
            thread: prepared.thread.clone(),
            process_thread: Some(prepared),
            dormant: Some(dormant),
        })
    }

    // Finish constructing the large architectural register image before
    // entering scheduler preparation. Keeping this frame separate leaves
    // room for IRQ entry, preemption and switch-tail retirement on this stack.
    #[inline(never)]
    fn prepare_user_execution(
        address_space: FallibleArc<NativeAddressSpace>,
        start: UserThreadStart,
        argument: u64,
    ) -> Result<alloc::boxed::Box<core::cell::UnsafeCell<UserExecution>>, ProcessError> {
        // Match the architectural Result directly: mapping its error before
        // extracting the context creates extra full-register-image temporaries
        // in the kernel build profile.
        match crate::hal::user::prepare_context(
            start.entry().get(),
            start.stack().get(),
            start.tls().get(),
        ) {
            Ok(mut context) => {
                context.set_entry_argument(argument);
                UserExecution::try_new(address_space, context)
                    .map_err(|()| ProcessError::Allocation)
            }
            Err(error) => Err(ProcessError::UserEntry(error)),
        }
    }

    fn abort_pending_thread(&self) {
        let became_stopped = match self
            .inner
            .state
            .with(|state| state.lifecycle.abort_thread())
        {
            Ok(became_stopped) => became_stopped,
            Err(_) => process_invariant_violation(),
        };
        if became_stopped {
            self.publish_stopped();
        }
    }

    fn prepare_user_thread_after_admission(&self) -> Result<PreparedUserThread, ProcessError> {
        let record_charge = self
            .inner
            .domain
            .reserve(metadata_amount::<ThreadRecord>()?)?
            .commit();
        let record = FallibleArc::try_new(ThreadRecord {
            active: AtomicBool::new(false),
            scheduler_id: AtomicU64::new(0),
            next: ProcessLock::new(None),
            _metadata_charge: record_charge,
        })
        .map_err(|_| ProcessError::Allocation)?;
        let thread_metadata_bytes = UserThread::allocation_size()
            .and_then(|bytes| u64::try_from(bytes).ok())
            .ok_or(ProcessError::Allocation)?;
        let thread_metadata = self
            .inner
            .domain
            .reserve(
                ResourceAmount::ZERO
                    .with(ResourceKind::KernelObjects, 1)
                    .with(ResourceKind::KernelMemoryBytes, thread_metadata_bytes),
            )?
            .commit();
        let thread = UserThread::try_prepared(self, thread_metadata)?;
        let stack_bytes = u64::try_from(crate::mm::stack::thread_stack_bytes())
            .map_err(|_| ProcessError::Allocation)?;
        let thread_object_bytes = u64::try_from(crate::task::thread::Thread::allocation_size())
            .map_err(|_| ProcessError::Allocation)?;
        let user_execution_bytes = u64::try_from(UserExecution::allocation_size())
            .map_err(|_| ProcessError::Allocation)?;
        let execution_bytes = stack_bytes
            .checked_add(thread_object_bytes)
            .and_then(|bytes| bytes.checked_add(user_execution_bytes))
            .ok_or(ProcessError::Allocation)?;
        let stack_pages = stack_bytes / hyper::mm::PAGE_SIZE;
        let execution_charge = self
            .inner
            .domain
            .reserve(
                ResourceAmount::ZERO
                    .with(ResourceKind::Threads, 1)
                    .with(ResourceKind::KernelMemoryBytes, execution_bytes)
                    .with(ResourceKind::CommittedPages, stack_pages),
            )?
            .commit();
        let address_space = self.inner.state.with(|state| {
            state
                .address_space
                .as_ref()
                .cloned()
                .ok_or(ProcessError::AddressSpaceReferenced)
        })?;
        Ok(PreparedUserThread {
            process: self.clone(),
            thread,
            record: Some(record),
            execution_charge: Some(execution_charge),
            address_space,
            committed: false,
        })
    }

    pub(super) fn install_retention(&self, retention: FallibleArc<super::retention::Retention>) {
        if self.inner.retention.publish(retention).is_err() {
            process_invariant_violation();
        }
    }

    /// Compares live retention ownership, not a historical diagnostic KOID.
    pub(crate) fn is_retention_target(
        &self,
        control: &super::retention::ProcessRetirement,
    ) -> bool {
        self.inner
            .retention
            .get()
            .is_some_and(|retention| core::ptr::eq(&**retention, &*control.retention))
    }

    pub(super) fn retention_budget(
        &self,
    ) -> Result<FallibleArc<super::retention::SupervisorBudget>, super::retention::Error> {
        if let Some(budget) = self.inner.retention_budget.with(|slot| slot.clone()) {
            return Ok(budget);
        }
        let mut prepared = Some(super::retention::SupervisorBudget::new(&self.inner.domain)?);
        let budget = self.inner.retention_budget.with(|slot| {
            if slot.is_none() {
                *slot = prepared.take();
            }
            match slot.as_ref() {
                Some(budget) => budget.clone(),
                None => process_invariant_violation(),
            }
        });
        drop(prepared);
        Ok(budget)
    }

    pub(crate) fn request_fault_stop(
        &self,
        reason: TerminalReason,
        pc: u64,
        address: u64,
        syndrome: u64,
    ) -> ProcessStopReport {
        self.request_stop_inner(reason, Some((pc, address, syndrome)))
    }

    pub(crate) fn request_stop(&self, reason: TerminalReason) -> ProcessStopReport {
        self.request_stop_inner(reason, None)
    }

    /// Observes process-wide stop before the asynchronous per-Thread fan-out.
    /// The common running path takes no Process lock. A published stop retains
    /// its first terminal reason through retirement; Processes never restart.
    pub(crate) fn requested_stop_reason(&self) -> Option<TerminalReason> {
        if !self.inner.stop_requested.load(Ordering::Acquire) {
            return None;
        }
        self.inner.state.with(|state| state.lifecycle.terminal())
    }

    fn request_stop_inner(
        &self,
        reason: TerminalReason,
        fault: Option<(u64, u64, u64)>,
    ) -> ProcessStopReport {
        let (newly_requested, completed, effective_reason, pending_threads) =
            self.inner.state.with(|state| {
                let before = state.lifecycle.phase();
                let newly_requested = state.lifecycle.request_stop(reason);
                let effective_reason = match state.lifecycle.terminal() {
                    Some(reason) => reason,
                    None => process_invariant_violation(),
                };
                // Publish before unlocking closed handle admission. A syscall
                // rejected during the fan-out must not return an error to EL0
                // merely because this Thread has not received its stop yet.
                self.inner.stop_requested.store(true, Ordering::Release);
                if newly_requested && let Some(retention) = self.inner.retention.get() {
                    // Publish the winning fault and stop deadline before a
                    // concurrent final detach can expose Stopped to the reaper.
                    // These metadata-only operations never reenter Process.
                    if let Some((pc, address, syndrome)) = fault {
                        retention.fault(pc, address, syndrome);
                    }
                    retention.stopping(effective_reason);
                }
                (
                    newly_requested,
                    before != ProcessPhase::Stopped
                        && state.lifecycle.phase() == ProcessPhase::Stopped,
                    effective_reason,
                    state.lifecycle.pending_threads(),
                )
            });
        if newly_requested {
            if let Some(address) = self.inner.state.with(|state| state.address_space.clone()) {
                address.begin_process_stop();
            }
            queue_retirement(self.clone());
            crate::reaper::request();
        }
        if completed {
            self.publish_stopped();
        }
        let mut current = self.inner.state.with(|state| state.threads.clone());
        let mut dispatched_threads = 0usize;
        let mut dispatch = StopDispatchProgress::new(pending_threads);
        while let Some(record) = current {
            current = record.next.with(|next| next.clone());
            if !record.active.load(Ordering::Acquire) {
                continue;
            }
            let raw = record.scheduler_id.load(Ordering::Relaxed);
            if raw == 0 {
                dispatch.observe(false);
                continue;
            }
            dispatched_threads = dispatched_threads.saturating_add(1);
            let id = ThreadId::from_process_publication(raw);
            dispatch.observe(scheduler::request_user_thread_stop(id, effective_reason).is_ok());
        }
        ProcessStopReport {
            newly_requested,
            dispatched_threads,
            dispatch_complete: dispatch.is_complete(),
        }
    }

    /// Checks cancellation at a reversible memory-transaction retry boundary.
    /// The caller has dropped every unpublished transaction and execution pin.
    pub(crate) fn retry_user_memory_conflict(&self) -> Result<(), ProcessError> {
        drop(self.address_space_owner()?);
        let caller =
            scheduler::current_user_thread()?.filter(|thread| thread.process_id() == self.id());
        let cancelled = || {
            caller.as_ref().is_some_and(|thread| {
                thread.snapshot().phase == super::UserThreadPhase::StopRequested
            })
        };
        if cancelled() {
            return Err(ProcessError::Lifecycle(LifecycleError::AdmissionClosed));
        }
        scheduler::yield_now()?;
        drop(self.address_space_owner()?);
        if cancelled() {
            return Err(ProcessError::Lifecycle(LifecycleError::AdmissionClosed));
        }
        Ok(())
    }

    /// Retries copies/materialization after releasing its complete prepared
    /// transaction. Thread and Process stop cancel contention without CPU pins.
    pub(super) fn retry_user_memory<T>(
        &self,
        mut operation: impl FnMut(FallibleArc<NativeAddressSpace>) -> Result<T, MachineError>,
    ) -> Result<T, ProcessError> {
        loop {
            let address_space = self.address_space_owner()?;
            match operation(address_space) {
                Ok(value) => return Ok(value),
                Err(error) if error.is_mapping_conflict() => self.retry_user_memory_conflict()?,
                Err(error) => return Err(ProcessError::UserMemory(error)),
            }
        }
    }

    pub(crate) fn copy_to_user(
        &self,
        destination: UserSlice,
        source: &[u8],
    ) -> Result<(), ProcessError> {
        self.retry_user_memory(|address_space| address_space.copy_to_user(destination, source))
    }

    pub(crate) fn copy_from_user(
        &self,
        source: UserSlice,
        destination: &mut [u8],
    ) -> Result<(), ProcessError> {
        // Eviction may win between file population and the resident copy.
        // Retrying only overwrites our private buffer, without a user-visible
        // side effect; the ordinary retry boundary still checks cancellation.
        self.retry_user_memory(|address_space| address_space.copy_from_user(source, destination))
    }

    /// Resolves file faults before I/O locks and retains only the selected
    /// physical owners. The subsequent copy cannot fault back into a filesystem.
    pub(crate) fn prepare_user_read(
        &self,
        source: UserSlice,
    ) -> Result<NativeUserRead, ProcessError> {
        self.retry_user_memory(|address_space| address_space.prepare_user_read(source))
    }

    /// Pins one exact output range across a capability publication transaction.
    pub(crate) fn reserve_user_write(
        &self,
        destination: UserSlice,
    ) -> Result<UserWriteReservation, ProcessError> {
        self.retry_user_memory(|address_space| {
            NativeAddressSpace::reserve_user_write(address_space, destination)
        })
    }

    /// Replaces a caller-owned batch without returning copies of its plans.
    /// Materializes every destination before pinning any output. A competing
    /// remap may still win; then the whole local reservation batch is dropped
    /// before retry, so one output cannot prevent another output's COW commit.
    pub(crate) fn reserve_user_writes_into<const N: usize>(
        &self,
        destinations: [Option<UserSlice>; N],
        mut writes: [&mut Option<UserWriteReservation>; N],
    ) -> Result<(), ProcessError> {
        for write in &mut writes {
            drop(write.take());
        }
        self.retry_user_memory(|address_space| {
            for destination in destinations
                .iter()
                .flatten()
                .filter(|range| range.length() != 0)
            {
                address_space.resolve_private_write(*destination)?;
            }
            for (slot, destination) in writes.iter_mut().zip(destinations) {
                if let Some(destination) = destination.filter(|range| range.length() != 0) {
                    match NativeAddressSpace::reserve_user_write(address_space.clone(), destination)
                    {
                        Ok(write) => **slot = Some(write),
                        Err(error) => {
                            // Nothing may remain pinned across a retry, even
                            // when only the last destination lost admission.
                            for write in &mut writes {
                                drop(write.take());
                            }
                            return Err(error);
                        }
                    }
                }
            }
            Ok(())
        })
    }

    fn retire(&self) -> Result<ProcessRetirementStep, ProcessError> {
        if let Some(retention) = self.inner.retention.get()
            && retention.released()
        {
            retention.discard_released();
        }
        let phase = self.inner.state.with(|state| state.lifecycle.phase());
        // Withdrawing active handles does not destroy resolved operation pins.
        // In-flight transactions retain their reserved slots until rollback;
        // unrelated endpoints close even while a filesystem call is draining.
        if phase == ProcessPhase::Stopping {
            for _ in 0..64 {
                let detached = self.inner.state.with(|state| {
                    let (metadata, owner) = self.inner.handles.with(|table| {
                        table.remove_stopping_owner(&mut state.stopping_handle_cursor)
                    })?;
                    // An in-flight syscall can reclaim empty handle pages while
                    // unwinding. Withdraw the matching accounting index under
                    // the same Process lock before exposing an empty slot page.
                    let (charge, record) = state.handle_accounting.release(metadata.value);
                    Some((metadata, owner, charge, record))
                });
                let Some((metadata, owner, charge, record)) = detached else {
                    break;
                };
                if let Some(retention) = self.inner.retention.get() {
                    retention.handle(metadata);
                }
                owner.complete();
                drop(charge);
                drop(record);
            }
            return Ok(ProcessRetirementStep::InProgress);
        }
        if self.inner.state.with(|state| state.address_retired) {
            if self.retention_holds() {
                return Ok(ProcessRetirementStep::InProgress);
            }
            self.finish_retirement();
            return Ok(ProcessRetirementStep::Complete);
        }
        if phase == ProcessPhase::Stopped {
            // Capture remaining occupied metadata before the final table seal.
            if let Some(retention) = self.inner.retention.get() {
                let mut scan = crate::capability::HandleScanCursor::from_token(0);
                loop {
                    let page = self.scan_handles(scan)?;
                    for metadata in page.entries() {
                        retention.handle(*metadata);
                    }
                    match page.next() {
                        Some(next) => scan = next,
                        None => break,
                    }
                }
            }
            let mut cursor = self.inner.state.with(|state| {
                let cursor = self.inner.handles.with(HandleTable::begin_teardown)?;
                state.lifecycle.begin_retirement()?;
                Ok::<_, ProcessError>(cursor)
            })?;
            loop {
                let closed = self
                    .inner
                    .handles
                    .with(|table| table.remove_next(&mut cursor));
                let Some(closed) = closed else {
                    break;
                };
                closed.complete();
            }
            self.inner
                .handles
                .with(|table| table.finish_teardown(cursor));
            self.inner.state.with(|state| state.handles_retired = true);
            if self.inner.retention.get().is_some() {
                // Let final object destruction run before testing exclusive
                // VMO ownership. Closed handles no longer carry authority,
                // but their queued payloads may still hold conservative refs.
                return Ok(ProcessRetirementStep::InProgress);
            }
        } else if phase != ProcessPhase::Retiring {
            return Err(ProcessError::Lifecycle(LifecycleError::NotStopped));
        } else if !self.inner.state.with(|state| state.handles_retired) {
            return Ok(ProcessRetirementStep::InProgress);
        }

        // A transferred VMAR can keep the address-space owner alive after the
        // process exits. It must not keep this process's content protections
        // alive: withdraw file sources after every thread has stopped, before
        // waiting for external references to permit final retirement. This
        // does not allocate or depend on quota headroom for a new page table.
        let mapping_owner = self.inner.state.with(|state| state.address_space.clone());
        if let Some(address_space) = mapping_owner {
            address_space.begin_process_stop();
            address_space.retire_file_sources();
            if let Some(retention) = self.inner.retention.get() {
                retention.capture(&address_space);
            }
        }
        let address_space = self.inner.state.with(|state| {
            if !state.handles_retired {
                process_invariant_violation();
            }
            state.address_space.take()
        });
        let Some(address_space) = address_space else {
            return Ok(ProcessRetirementStep::InProgress);
        };
        let address_space = match address_space.try_into_unique() {
            Ok(address_space) => address_space,
            Err(address_space) => {
                self.inner
                    .state
                    .with(|state| state.address_space = Some(address_space));
                return Ok(ProcessRetirementStep::PendingReferences);
            }
        };
        match NativeAddressSpace::retire(address_space) {
            Ok(()) => {
                self.inner.state.with(|state| state.address_retired = true);
                if self.retention_holds() {
                    return Ok(ProcessRetirementStep::InProgress);
                }
                self.finish_retirement();
                Ok(ProcessRetirementStep::Complete)
            }
            Err(failure) => {
                let (_, address_space) = failure.into_parts();
                Ok(ProcessRetirementStep::Retry(AddressSpaceRetirement {
                    process: self.clone(),
                    address_space: Some(address_space),
                    failed_attempts: 1,
                }))
            }
        }
    }

    pub(super) fn with_run_admission<R>(
        &self,
        expected_image_generation: u64,
        operation: impl FnOnce() -> R,
    ) -> Result<R, super::user_thread::RunAdmissionError> {
        self.inner.state.with(|state| {
            if state.lifecycle.phase() != ProcessPhase::Running {
                return Err(super::user_thread::RunAdmissionError::AdmissionClosed);
            }
            if expected_image_generation != self.image_generation() {
                return Err(super::user_thread::RunAdmissionError::StaleImage);
            }
            Ok(operation())
        })
    }

    fn retention_holds(&self) -> bool {
        match self.inner.retention.get() {
            Some(retention) if retention.released() => {
                retention.discard_released();
                false
            }
            Some(_) => true,
            None => false,
        }
    }

    fn finish_retirement(&self) {
        let retired_table_storage = self.inner.handles.with(HandleTable::take_retired_storage);
        let (membership, process_charge, mut records, mut handle_charges, table_charges) =
            self.inner.state.with(|state| {
                if state.lifecycle.phase() != ProcessPhase::Retiring {
                    process_invariant_violation();
                }
                (
                    state.group_membership.take(),
                    state.process_charge.take(),
                    state.threads.take(),
                    state.handle_accounting.take_records(),
                    state.handle_table_charge.take(),
                )
            });
        while let Some(record) = records {
            records = record.next.with(Option::take);
            drop(record);
        }
        let index = self
            .inner
            .state
            .with(|state| state.handle_accounting.take_index());
        drop(index);
        while let Some(record) = handle_charges {
            handle_charges = record.next.with(Option::take);
            let charges = record.state.with(|state| {
                let mut charges = alloc::vec::Vec::new();
                core::mem::swap(&mut charges, &mut state.entries);
                charges
            });
            drop(charges);
            drop(record);
        }
        drop(retired_table_storage);
        drop(table_charges);
        let membership = match membership {
            Some(membership) => membership,
            None => process_invariant_violation(),
        };
        drop(process_charge);
        // Retired is an observation of completed cleanup, not its start.
        self.inner.state.with(|state| {
            if state.lifecycle.finish_retirement().is_err() {
                process_invariant_violation();
            }
        });
        let final_snapshot = self.snapshot();
        match self.inner.object.get() {
            Some(object) => object.object().publish_final_snapshot(final_snapshot),
            None => process_invariant_violation(),
        }
        // Group termination observes completed Process cleanup and its final snapshot.
        membership.retire();
    }

    /// Publishes object-visible termination before waking Process joiners.
    fn publish_stopped(&self) {
        if let Some(retention) = self.inner.retention.get()
            && let Some(reason) = self.inner.state.with(|state| state.lifecycle.terminal())
        {
            retention.stopping(reason);
        }
        match self.inner.object.get() {
            Some(object) => object.object().publish_stopped(),
            None => process_invariant_violation(),
        }
        if self.inner.stopped.complete_all().is_err() {
            process_invariant_violation();
        }
        queue_retirement(self.clone());
        crate::reaper::request();
    }
}

impl Clone for Process {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

#[must_use = "publish or drop the dormant initial Thread"]
struct PreparedInitialUserThread {
    thread: UserThread,
    process_thread: Option<PreparedUserThread>,
    dormant: Option<crate::task::scheduler::DormantUserThread>,
}

impl PreparedInitialUserThread {
    fn publish(mut self) -> (UserThread, ThreadId, Option<TerminalReason>) {
        let dormant = match self.dormant.take() {
            Some(dormant) => dormant,
            None => process_invariant_violation(),
        };
        let id = dormant.id();
        let process_thread = match self.process_thread.take() {
            Some(process_thread) => process_thread,
            None => process_invariant_violation(),
        };
        let terminal = process_thread.publish(id, dormant);
        (self.thread.clone(), id, terminal)
    }
}

struct PreparedUserThread {
    process: Process,
    thread: UserThread,
    record: Option<FallibleArc<ThreadRecord>>,
    execution_charge: Option<CommittedCharge>,
    address_space: FallibleArc<NativeAddressSpace>,
    committed: bool,
}

impl PreparedUserThread {
    fn publish(
        mut self,
        id: ThreadId,
        dormant: crate::task::scheduler::DormantUserThread,
    ) -> Option<TerminalReason> {
        let record = match self.record.take() {
            Some(record) => record,
            None => process_invariant_violation(),
        };
        let charge = match self.execution_charge.take() {
            Some(charge) => charge,
            None => process_invariant_violation(),
        };
        let process_record = record.clone();
        let membership = ProcessThreadMembership {
            process: self.process.clone(),
            record: Some(record),
        };
        dormant.commit_before_process_publication(UserExecutionOwnership::new(membership, charge));
        self.thread.publish(id);
        let terminal = self.process.inner.state.with(|state| {
            if state.lifecycle.publish_thread().is_err() {
                process_invariant_violation();
            }
            process_record
                .next
                .with(|next| *next = state.threads.clone());
            process_record
                .scheduler_id
                .store(id.get(), Ordering::Relaxed);
            process_record.active.store(true, Ordering::Release);
            state.threads = Some(process_record);
            state.lifecycle.terminal()
        });
        self.committed = true;
        terminal
    }
}

impl Drop for PreparedUserThread {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        self.process.abort_pending_thread();
    }
}

pub(super) struct ProcessThreadMembership {
    process: Process,
    record: Option<FallibleArc<ThreadRecord>>,
}

impl ProcessThreadMembership {
    pub(super) const fn process(&self) -> &Process {
        &self.process
    }

    pub(super) fn detach(
        mut self,
        terminal: TerminalReason,
        koid: u64,
        context: &crate::hal::user::UserContext,
    ) {
        let record = match self.record.take() {
            Some(record) => record,
            None => process_invariant_violation(),
        };
        if !record.active.swap(false, Ordering::AcqRel) {
            process_invariant_violation();
        }
        let status = match terminal {
            TerminalReason::ThreadExited { status }
            | TerminalReason::ProcessExited { status }
            | TerminalReason::LastThreadExited { status } => status,
            _ => 0,
        };
        let detached = self.process.inner.state.with(|state| {
            let before = state.lifecycle.phase();
            // Select final-thread evidence in the same critical section as
            // detachment. Concurrent normal exits must not both observe two
            // live threads and then omit the actual last thread's registers.
            if (state.lifecycle.terminal().is_some() || state.lifecycle.active_threads() == 1)
                && let Some(retention) = self.process.inner.retention.get()
            {
                retention.thread(koid, context);
            }
            state.lifecycle.detach_thread(status)?;
            let detached = unlink_thread_record(state, &record);
            Ok::<_, LifecycleError>((
                before != ProcessPhase::Stopped && state.lifecycle.phase() == ProcessPhase::Stopped,
                detached,
            ))
        });
        let (became_stopped, detached_record) = match detached {
            Ok(result) => result,
            Err(_) => process_invariant_violation(),
        };
        drop(detached_record);
        drop(record);
        if became_stopped {
            self.process.publish_stopped();
        }
    }
}

impl Drop for ProcessThreadMembership {
    fn drop(&mut self) {
        if self.record.is_some() {
            process_invariant_violation();
        }
    }
}

#[must_use = "retry or deliberately retain committed process retirement"]
pub(crate) struct AddressSpaceRetirement {
    process: Process,
    address_space: Option<UniqueFallibleArc<NativeAddressSpace>>,
    failed_attempts: u32,
}

impl AddressSpaceRetirement {
    // Brief transport contention is expected during teardown. Report only
    // persistent failure, once, and pair its warning with eventual recovery.
    const WARNING_THRESHOLD: u32 = 3;

    pub(crate) fn retry(mut self) -> Result<(), (Self, MachineError)> {
        if let Some(retention) = self.process.inner.retention.get()
            && retention.released()
        {
            retention.discard_released();
        }
        let address_space = match self.address_space.take() {
            Some(address_space) => address_space,
            None => process_invariant_violation(),
        };
        match NativeAddressSpace::retire(address_space) {
            Ok(()) => {
                self.process
                    .inner
                    .state
                    .with(|state| state.address_retired = true);
                if !self.process.retention_holds() {
                    self.process.finish_retirement();
                }
                if self.failed_attempts >= Self::WARNING_THRESHOLD {
                    crate::pr_info!(
                        "HypeR: Process {:?} address-space retirement completed after {} failed attempts",
                        self.process.id(),
                        self.failed_attempts
                    );
                }
                Ok(())
            }
            Err(failure) => {
                let (error, address_space) = failure.into_parts();
                self.address_space = Some(address_space);
                self.failed_attempts = self.failed_attempts.saturating_add(1);
                if self.failed_attempts == Self::WARNING_THRESHOLD {
                    crate::pr_warn!(
                        "HypeR: Process {:?} address-space retirement deferred after {} failed attempts: {error:?}; retaining resources and retrying",
                        self.process.id(),
                        self.failed_attempts
                    );
                }
                Err((self, error))
            }
        }
    }
}

impl Drop for AddressSpaceRetirement {
    fn drop(&mut self) {
        if self.address_space.is_some() {
            process_invariant_violation();
        }
    }
}

fn metadata_amount<T>() -> Result<ResourceAmount, ProcessError> {
    Ok(ResourceAmount::ZERO
        .with(ResourceKind::KernelObjects, 1)
        .with(
            ResourceKind::KernelMemoryBytes,
            u64::try_from(FallibleArc::<T>::allocation_size())
                .map_err(|_| ProcessError::Allocation)?,
        ))
}

fn process_metadata_amount() -> Result<ResourceAmount, ProcessError> {
    let bytes = FallibleArc::<ProcessInner>::allocation_size()
        .checked_add(super::directory::registration_size())
        .and_then(|bytes| u64::try_from(bytes).ok())
        .ok_or(ProcessError::Allocation)?;
    Ok(ResourceAmount::ZERO
        .with(ResourceKind::KernelObjects, 1)
        .with(ResourceKind::KernelMemoryBytes, bytes))
}

fn unlink_thread_record(
    state: &mut ProcessState,
    target: &FallibleArc<ThreadRecord>,
) -> FallibleArc<ThreadRecord> {
    let mut current = state.threads.clone();
    let mut previous: Option<FallibleArc<ThreadRecord>> = None;
    while let Some(record) = current {
        let next = record.next.with(|next| next.clone());
        if core::ptr::eq::<ThreadRecord>(&*record, &**target) {
            if let Some(previous) = previous {
                previous.next.with(|link| *link = next);
            } else {
                state.threads = next;
            }
            // Keep the published successor immutable for lock-free stop
            // traversals which may already retain this detached record.
            // The returned owner is dropped outside the Process lock; once
            // the last traversal releases it, its successor reference follows.
            return record;
        }
        previous = Some(record);
        current = next;
    }
    process_invariant_violation()
}

fn allocate_process_id() -> Result<ProcessId, ProcessError> {
    let mut current = NEXT_PROCESS_ID.load(Ordering::Relaxed);
    loop {
        if current == 0 {
            return Err(ProcessError::Allocation);
        }
        let next = current.checked_add(1).ok_or(ProcessError::Allocation)?;
        match NEXT_PROCESS_ID.compare_exchange_weak(
            current,
            next,
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => return Ok(ProcessId(current)),
            Err(observed) => current = observed,
        }
    }
}

fn create_failure(
    error: ProcessError,
    address_space: UniqueFallibleArc<NativeAddressSpace>,
) -> ProcessCreateFailure {
    ProcessCreateFailure {
        error: Some(error),
        address_space: Some(address_space),
    }
}

fn create_failure_from_arc(
    error: ProcessError,
    address_space: FallibleArc<NativeAddressSpace>,
) -> ProcessCreateFailure {
    match address_space.try_into_unique() {
        Ok(address_space) => create_failure(error, address_space),
        Err(_) => process_invariant_violation(),
    }
}

fn recover_unpublished_address_space(process: Process) -> FallibleArc<NativeAddressSpace> {
    // Keep the large ProcessInner in its existing allocation. Unique ownership
    // proves no lifecycle operation can race the extraction; destruction runs
    // after releasing the state lock and never moves the whole state to stack.
    let inner = match process.inner.try_into_unique() {
        Ok(inner) => inner,
        Err(_) => process_invariant_violation(),
    };
    let address_space = inner.state.with(|state| state.address_space.take());
    drop(inner);
    match address_space {
        Some(address_space) => address_space,
        None => process_invariant_violation(),
    }
}

#[cold]
fn process_invariant_violation() -> ! {
    hyper::debug::invariant_failure("process::owner::process_invariant_violation invariant")
}
