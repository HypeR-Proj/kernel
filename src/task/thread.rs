// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Architecture-neutral thread objects and execution payloads.

use alloc::boxed::Box;
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicU64, Ordering};
use hyper::cpu::CpuIndex;

// ProcessBuilder commits one identity label for both the Process and its
// initial Thread. Keep the scheduler snapshot capacity aligned with that ABI
// contract so a name accepted by the builder cannot fail later at publication.
const _: () = assert!(hyper::abi::native::HYPER_NATIVE_PROCESS_NAME_MAX_BYTES <= usize::MAX as u64);
pub(crate) const MAX_THREAD_NAME_BYTES: usize =
    hyper::abi::native::HYPER_NATIVE_PROCESS_NAME_MAX_BYTES as usize;
const _: () =
    assert!(MAX_THREAD_NAME_BYTES as u64 == hyper::abi::native::HYPER_NATIVE_THREAD_NAME_MAX_BYTES);

use crate::mm::stack::KernelStack;
use crate::task::wait::WaitRecord;
use {
    crate::task::policy::BudgetParameters, crate::task::policy::CpuMask,
    crate::task::policy::SchedulingClass, crate::task::policy::SchedulingParameters,
    crate::task::policy::SchedulingPolicy, crate::task::policy::ThreadPlacement,
    crate::task::policy::ThreadPriority,
};
use {
    crate::task::scheduler::algorithms::BudgetRuntime,
    crate::task::scheduler::algorithms::FairRuntime,
    crate::task::scheduler::algorithms::PolicyRuntime,
};
use {
    crate::task::thread_object::ThreadObject, crate::task::thread_object::ThreadObjectSnapshot,
    crate::task::thread_object::ThreadRole,
};

pub type KernelThreadEntry = extern "C" fn(usize);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ThreadRetirementError {
    PublicationRejected,
}

/// Type-erased completion owned after a subsystem execution is detached.
pub(crate) trait ThreadRetirementAction: Send {
    fn complete(self: Box<Self>) -> Result<(), ThreadRetirementError>;
}

#[must_use = "detached execution retirement must be completed"]
pub(crate) struct ThreadRetirement {
    action: Box<dyn ThreadRetirementAction>,
    _resource_charge: Option<crate::accounting::CommittedCharge>,
}

/// Subsystem hook for a scheduler-owned external execution allocation.
pub(crate) trait ExternalThreadExecutionLifecycle: Send {
    fn take_thread_retirement(&mut self) -> Option<ThreadRetirement>;
}

/// Compiler-managed type erasure for one scheduler-owned execution cell.
trait ErasedExternalThreadExecution: Send {
    fn take_thread_retirement(&mut self) -> Option<ThreadRetirement>;
}

impl<T> ErasedExternalThreadExecution for UnsafeCell<T>
where
    T: ExternalThreadExecutionLifecycle + 'static,
{
    fn take_thread_retirement(&mut self) -> Option<ThreadRetirement> {
        self.get_mut().take_thread_retirement()
    }
}

/// Type-erased, uniquely owned execution payload with a stable address.
pub(crate) struct ExternalThreadExecution {
    pointer: super::external_execution::ExternalExecutionPointer,
    owner: Box<dyn ErasedExternalThreadExecution>,
}

// SAFETY: `owner` proves the erased payload is Send and keeps the allocation
// fixed. The copied pointer is never dereferenced merely by moving this unique
// wrapper; scheduler activation establishes exclusive access after migration.
unsafe impl Send for ExternalThreadExecution {}

impl ExternalThreadExecution {
    pub(crate) fn from_box<T>(payload: Box<UnsafeCell<T>>) -> Self
    where
        T: ExternalThreadExecutionLifecycle + 'static,
    {
        let pointer = core::ptr::NonNull::from(payload.as_ref());
        Self {
            pointer: super::external_execution::ExternalExecutionPointer::from_cell(pointer),
            owner: payload,
        }
    }

    fn pointer(&self) -> super::external_execution::ExternalExecutionPointer {
        self.pointer
    }

    fn take_retirement(&mut self) -> Option<ThreadRetirement> {
        self.owner.take_thread_retirement()
    }
}

impl ThreadRetirement {
    pub(crate) fn from_box<T>(action: Box<T>) -> Self
    where
        T: ThreadRetirementAction + 'static,
    {
        Self {
            action,
            _resource_charge: None,
        }
    }

    pub(super) fn complete(self) -> Result<(), ThreadRetirementError> {
        let Self {
            action,
            _resource_charge,
        } = self;
        action.complete()
    }

    fn retain_charge(&mut self, charge: crate::accounting::CommittedCharge) {
        if self._resource_charge.replace(charge).is_some() {
            hyper::debug::invariant_failure("task::thread::retain_charge invariant");
        }
    }
}

#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
struct RetirementAccountingProbe {
    domain: crate::accounting::ResourceDomain,
    baseline: u64,
    retained: u64,
}

#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
impl ThreadRetirementAction for RetirementAccountingProbe {
    fn complete(self: Box<Self>) -> Result<(), ThreadRetirementError> {
        let observed = self
            .domain
            .usage()
            .total(crate::accounting::ResourceKind::KernelMemoryBytes);
        if self.baseline.checked_add(self.retained) == Some(observed) {
            Ok(())
        } else {
            Err(ThreadRetirementError::PublicationRejected)
        }
    }
}

/// Verifies that an extracted retirement action remains charged while its
/// terminal callback executes and releases the charge immediately afterward.
#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
pub(crate) fn verify_retirement_charge_lifetime() -> bool {
    use {
        crate::accounting::ResourceAmount, crate::accounting::ResourceDomain,
        crate::accounting::ResourceKind, crate::accounting::ResourceLimits,
    };

    let domain = match ResourceDomain::try_new_root(ResourceLimits::UNLIMITED) {
        Ok(domain) => domain,
        Err(_) => return false,
    };
    let baseline = domain.usage().total(ResourceKind::KernelMemoryBytes);
    let retained = 64_u64;
    let charge = match domain
        .reserve(ResourceAmount::ZERO.with(ResourceKind::KernelMemoryBytes, retained))
    {
        Ok(reservation) => reservation.commit(),
        Err(_) => return false,
    };
    let action = match hyper::mm::try_box(RetirementAccountingProbe {
        domain: domain.clone(),
        baseline,
        retained,
    }) {
        Ok(action) => action,
        Err(_) => return false,
    };
    let mut retirement = ThreadRetirement::from_box(action);
    retirement.retain_charge(charge);
    let charged = baseline.checked_add(retained)
        == Some(domain.usage().total(ResourceKind::KernelMemoryBytes));
    let completed = retirement.complete().is_ok();
    charged && completed && domain.usage().total(ResourceKind::KernelMemoryBytes) == baseline
}

/// Queue position to use if a running FIFO thread must leave the CPU after a
/// priority change. The value is replaced by every subsequent priority change
/// and consumed by the next scheduling decision.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum DeferredFifoPlacement {
    Head,
    Tail,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ThreadId(u64);

impl ThreadId {
    pub(super) const SLOT_BITS: u32 = 24;
    const SLOT_MASK: u64 = (1 << Self::SLOT_BITS) - 1;
    const IDENTITY_LIMIT: u64 = u64::MAX >> Self::SLOT_BITS;

    pub const BOOTSTRAP: Self = Self(0);

    /// Combines a never-reused identity with a reusable registry-slot hint.
    pub(super) const fn from_scheduler_parts(identity: u64, slot: usize) -> Option<Self> {
        if identity == 0 || identity > Self::IDENTITY_LIMIT || slot >= Self::SLOT_MASK as usize {
            return None;
        }
        Some(Self((identity << Self::SLOT_BITS) | (slot as u64 + 1)))
    }

    /// Reconstructs an ID retained by Process publication.
    pub(crate) const fn from_process_publication(value: u64) -> Self {
        Self(value)
    }

    /// Diagnostic lookup only; this ID never grants scheduling authority.
    #[cfg(feature = "kernel-trace")]
    pub(crate) const fn from_diagnostic(value: u64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u64 {
        self.0
    }

    /// Returns the private reusable slot encoded by the scheduler.
    pub(super) const fn scheduler_slot(self) -> Option<usize> {
        if self.0 == 0 {
            Some(0)
        } else {
            let encoded = self.0 & Self::SLOT_MASK;
            if encoded == 0 {
                None
            } else {
                Some((encoded - 1) as usize)
            }
        }
    }

    #[cfg(test)]
    pub(super) const fn for_test(identity: u64) -> Self {
        match Self::from_scheduler_parts(identity, 0) {
            Some(id) => id,
            None => Self::BOOTSTRAP,
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ThreadState {
    Dormant,
    Ready,
    Running,
    Idle,
    Blocked,
    /// Source switch is committed and may still execute; target publication awaits switch-tail.
    Migrating,
    Terminated,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum QueueMembership {
    None,
    ReadyRealTime { cpu: CpuIndex, priority: u8 },
    ReadyFair { cpu: CpuIndex },
    Waiting { queue: usize },
    Terminated,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct QueueLinks {
    pub previous: Option<ThreadId>,
    pub next: Option<ThreadId>,
    pub membership: QueueMembership,
}

/// Placement change retained by a Thread until its source context is stopped.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct MigrationRequest {
    pub target: CpuIndex,
    pub affinity: CpuMask,
}

impl QueueLinks {
    pub(crate) const EMPTY: Self = Self {
        previous: None,
        next: None,
        membership: QueueMembership::None,
    };
}

enum ThreadExecution {
    Kernel,
    Vcpu(ExternalThreadExecution),
    User(Box<UnsafeCell<crate::process::UserExecution>>),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExecutionKind {
    Kernel,
    Vcpu,
    User,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Allocation,
    NameTooLong,
    InvalidPlacement,
    ObjectAllocation,
    ObjectIdentityExhausted,
    ObjectRegistrationExhausted,
    VirtualInterrupt(crate::hal::vm::VirtualInterruptError),
}

impl From<crate::object::ObjectCreationError> for Error {
    fn from(error: crate::object::ObjectCreationError) -> Self {
        match error {
            crate::object::ObjectCreationError::Allocation => Self::ObjectAllocation,
            crate::object::ObjectCreationError::KoidExhausted => Self::ObjectIdentityExhausted,
            crate::object::ObjectCreationError::RegistrationExhausted => {
                Self::ObjectRegistrationExhausted
            }
        }
    }
}

impl From<crate::hal::vm::VirtualInterruptError> for Error {
    fn from(error: crate::hal::vm::VirtualInterruptError) -> Self {
        Self::VirtualInterrupt(error)
    }
}

/// A schedulable execution entity.
///
/// Every thread owns a kernel scheduling context and, except for the bootstrap
/// thread, a private kernel stack. vCPU architectural state is an attached
/// execution payload; it is deliberately separate from the context used while
/// the scheduler and exception handlers execute in the host hypervisor
/// privilege domain. A user execution payload strongly owns its Process and
/// prepared address space before it becomes a Thread variant.
pub struct Thread {
    identity: ThreadIdentity,
    object: ThreadObject,
    schedule_owner: ScheduleOwner,
    schedule: UnsafeCell<ThreadScheduleState>,
    /// Intrusive links protected by their `WaitQueue` lock plus a registry
    /// reader lane, or by exclusive coordination for lifecycle operations.
    /// Independent storage lets one queue link neighbors owned by different
    /// CPUs without borrowing their schedules.
    control_queue_links: UnsafeCell<QueueLinks>,
    /// Stable wait resources; arbitration stays in the CPU-owned schedule.
    wait_context: super::ThreadWaitContext,
    /// Monotonic execution nanoseconds charged while this Thread is current.
    runtime_ns: AtomicU64,
    resources: Box<ThreadResources>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ScheduleOwner {
    /// Owned by the transition coordinator and absent from every ready queue.
    Coordinator,
    /// Owned by one CPU domain, as current, ready, or blocked.
    Cpu(CpuIndex),
}

/// Stable identity with a separately synchronized diagnostic label.
struct ThreadIdentity {
    id: ThreadId,
    name: hyper::sync::InterruptSpinLock<ThreadNameSnapshot, crate::hal::irq::LocalMask>,
}

/// Intrusive ready-index links owned by the same scheduler transaction.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct ReadyTreeLinks {
    pub(super) left: Option<ThreadId>,
    pub(super) right: Option<ThreadId>,
    pub(super) height: u8,
}

impl ReadyTreeLinks {
    pub(super) const EMPTY: Self = Self {
        left: None,
        right: None,
        height: 1,
    };
}

/// State mutated only by scheduler transactions.
///
/// Keeping queue topology, placement, policy, and wait/migration state in one
/// explicit domain permits its ownership to move without moving Thread
/// identity or execution resources. Stored state is transition-lock-owned;
/// running state is owned by one CPU scheduler lock. The state has no internal
/// synchronization, so the linear residence token is the access authority.
pub(super) struct ThreadScheduleState {
    pub(super) placement: ThreadPlacement,
    pub(super) scheduling: SchedulingPolicy,
    pub(super) parameters: SchedulingParameters,
    pub(super) runtime: PolicyRuntime,
    pub(super) ready_turn: u64,
    pub(super) ready_tree: ReadyTreeLinks,
    pub(super) budget_history: Option<(BudgetRuntime, BudgetParameters)>,
    pub(super) fair_history: Option<(FairRuntime, u64)>,
    pub(super) fair_origin: Option<u128>,
    pub(super) auto_migrated: bool,
    pub(super) deferred_fifo_placement: Option<DeferredFifoPlacement>,
    pub(super) state: ThreadState,
    pub(super) ready_queue_links: QueueLinks,
    pub(super) wait: WaitRecord,
    pub(super) pending_migration: Option<MigrationRequest>,
}

impl ThreadScheduleState {
    pub(super) fn fair_slice_expired(&self) -> bool {
        matches!(self.runtime, PolicyRuntime::Fair(runtime) if runtime.remaining_ns() == 0)
    }

    pub(super) fn scheduling_class(&self) -> SchedulingClass {
        self.scheduling.class()
    }

    pub(super) fn replenish_fair_slice(&mut self) {
        if matches!(self.runtime, PolicyRuntime::Fair(_)) {
            self.runtime.finish_turn(self.parameters);
        }
    }

    pub(super) fn expire_fair_slice(&mut self) {
        if let PolicyRuntime::Fair(runtime) = &mut self.runtime {
            runtime.expire_turn();
        }
    }

    /// Retain each algorithm's ledger even while another algorithm is active.
    /// Parameter edits cannot mint a fresh turn or discard accounted execution.
    pub(super) fn install_parameters(&mut self, parameters: SchedulingParameters) {
        if self.parameters == parameters {
            return;
        }
        match self.runtime {
            PolicyRuntime::Budget(runtime) => {
                if let Some(budget) = self.parameters.budget_parameters() {
                    self.budget_history = Some((runtime, budget));
                }
            }
            PolicyRuntime::Fair(runtime) => {
                self.fair_history = Some((runtime, self.parameters.quantum_ns()));
            }
            _ => {}
        }
        self.runtime = match parameters.policy() {
            SchedulingPolicy::Budget => match (self.budget_history, parameters.budget_parameters())
            {
                (Some((mut runtime, old)), Some(new)) => {
                    if !runtime.reconfigure(old, new) {
                        hyper::debug::invariant_failure("scheduler budget period changed");
                    }
                    PolicyRuntime::Budget(runtime)
                }
                _ => PolicyRuntime::new(parameters),
            },
            SchedulingPolicy::Fair => match self.fair_history {
                Some((mut runtime, old_quantum)) => {
                    runtime.reconfigure(old_quantum, parameters.quantum_ns());
                    PolicyRuntime::Fair(runtime)
                }
                None => PolicyRuntime::new(parameters),
            },
            _ => PolicyRuntime::new(parameters),
        };
        self.scheduling = parameters.policy();
        self.parameters = parameters;
        self.deferred_fifo_placement = None;
    }
}

/// Stable machine resources governed by the running/stopped context protocol.
///
/// This value has its own private heap allocation. Mutating `Thread` identity
/// or scheduling state therefore cannot create an exclusive reference that
/// covers a machine pointer retained by assembly or an execution runner. The
/// Box is never replaced while the Thread is published; resource extraction
/// is permitted only after scheduler context ownership has stopped.
struct ThreadResources {
    /// Assembly owns this cell between switch preparation and switch tail.
    context: UnsafeCell<crate::hal::context::ThreadContext>,
    kernel_stack: Option<KernelStack>,
    execution: ThreadExecution,
    _ownership: Option<ThreadResourceOwnership>,
}

/// Accounting ownership whose lifetime is exactly one scheduler Thread.
///
/// Payload-producing subsystems reserve these generic resources before
/// construction. The scheduler consumes the object charge while publishing
/// the canonical Thread object and retains the execution charge until the
/// detached Thread is destroyed by the reaper. A separately admitted
/// retirement charge follows an extracted terminal action through completion,
/// so destroying the Thread never leaves deferred work unaccounted.
#[must_use = "thread resources must move into a scheduler Thread"]
pub(crate) struct ThreadResourceOwnership {
    _execution_charge: crate::accounting::CommittedCharge,
    retirement_charge: Option<crate::accounting::CommittedCharge>,
    object_charge: Option<crate::accounting::CommittedCharge>,
}

impl ThreadResourceOwnership {
    pub(crate) const fn new(
        execution_charge: crate::accounting::CommittedCharge,
        retirement_charge: crate::accounting::CommittedCharge,
        object_charge: crate::accounting::CommittedCharge,
    ) -> Self {
        Self {
            _execution_charge: execution_charge,
            retirement_charge: Some(retirement_charge),
            object_charge: Some(object_charge),
        }
    }

    fn take_object_charge(&mut self) -> crate::accounting::CommittedCharge {
        match self.object_charge.take() {
            Some(charge) => charge,
            None => hyper::debug::invariant_failure("task::thread::take_object_charge invariant"),
        }
    }

    fn take_retirement_charge(&mut self) -> crate::accounting::CommittedCharge {
        match self.retirement_charge.take() {
            Some(charge) => charge,
            None => {
                hyper::debug::invariant_failure("task::thread::take_retirement_charge invariant")
            }
        }
    }
}

impl Thread {
    pub(super) fn account_runtime_ns(&self, elapsed: u64) {
        self.runtime_ns.fetch_add(elapsed, Ordering::Relaxed);
    }

    pub(super) fn runtime_ns(&self) -> u64 {
        self.runtime_ns.load(Ordering::Relaxed)
    }

    pub(super) fn role(&self) -> ThreadRole {
        self.object.role()
    }

    pub(super) fn schedule_is_coordinator_owned(&self) -> bool {
        self.schedule_owner == ScheduleOwner::Coordinator
    }

    /// CPU owning this Thread's linear running-schedule token, if any.
    pub(super) fn schedule_owner_cpu(&self) -> Option<CpuIndex> {
        match self.schedule_owner {
            ScheduleOwner::Coordinator => None,
            ScheduleOwner::Cpu(cpu) => Some(cpu),
        }
    }

    /// Bounded scheduler storage charged before user-thread publication.
    pub(crate) const fn allocation_size() -> usize {
        core::mem::size_of::<Self>() + core::mem::size_of::<ThreadResources>()
    }

    /// Heap and guarded-stack bytes retained by one scheduler-owned vCPU.
    ///
    /// This excludes both the VM binding's shared aggregate and the canonical
    /// system Thread object. Each is independently refcounted and therefore
    /// owns accounting whose lifetime follows that allocation directly.
    pub(crate) const fn external_execution_allocation_size(payload_size: usize) -> Option<usize> {
        let bytes = Self::allocation_size();
        let bytes = match bytes.checked_add(payload_size) {
            Some(bytes) => bytes,
            None => return None,
        };
        let bytes = match bytes.checked_add(crate::mm::stack::thread_stack_bytes()) {
            Some(bytes) => bytes,
            None => return None,
        };
        Some(bytes)
    }

    fn allocate_resources(
        context: crate::hal::context::ThreadContext,
        kernel_stack: Option<KernelStack>,
        execution: ThreadExecution,
        ownership: Option<ThreadResourceOwnership>,
    ) -> Result<Box<ThreadResources>, Error> {
        hyper::mm::try_box(ThreadResources {
            context: UnsafeCell::new(context),
            kernel_stack,
            execution,
            _ownership: ownership,
        })
        .map_err(|_| Error::Allocation)
    }

    /// Assembles an unpublished Thread after each named constructor has
    /// prepared its identity, role and machine resources. This adds no fallible
    /// step after resource preparation and preserves coordinator ownership.
    fn from_parts(
        identity: ThreadIdentity,
        object: ThreadObject,
        placement: ThreadPlacement,
        scheduling: SchedulingPolicy,
        state: ThreadState,
        resources: Box<ThreadResources>,
    ) -> Self {
        Self {
            identity,
            object,
            schedule_owner: ScheduleOwner::Coordinator,
            schedule: UnsafeCell::new(ThreadScheduleState {
                placement,
                scheduling,
                parameters: super::scheduler::default_parameters(scheduling),
                runtime: PolicyRuntime::new(super::scheduler::default_parameters(scheduling)),
                ready_turn: 0,
                ready_tree: ReadyTreeLinks::EMPTY,
                budget_history: None,
                fair_history: None,
                fair_origin: None,
                auto_migrated: false,
                deferred_fifo_placement: None,
                state,
                ready_queue_links: QueueLinks::EMPTY,
                wait: WaitRecord::NEW,
                pending_migration: None,
            }),
            control_queue_links: UnsafeCell::new(QueueLinks::EMPTY),
            wait_context: super::ThreadWaitContext::new(),
            runtime_ns: AtomicU64::new(0),
            resources,
        }
    }

    pub(super) fn bootstrap(cpu_index: CpuIndex) -> Result<Self, Error> {
        Ok(Self::from_parts(
            ThreadIdentity {
                id: ThreadId::BOOTSTRAP,
                name: hyper::sync::InterruptSpinLock::new(ThreadNameSnapshot::per_cpu(
                    "bootstrap",
                    cpu_index,
                )?),
            },
            ThreadObject::try_system(ThreadId::BOOTSTRAP, ThreadRole::Bootstrap)?,
            ThreadPlacement::pinned(cpu_index),
            SchedulingPolicy::fair(),
            ThreadState::Running,
            Self::allocate_resources(
                crate::hal::context::ThreadContext::empty(),
                None,
                ThreadExecution::Kernel,
                None,
            )?,
        ))
    }

    pub(super) fn kernel(
        id: ThreadId,
        placement: ThreadPlacement,
        name: &str,
        entry: KernelThreadEntry,
        argument: usize,
    ) -> Result<Self, Error> {
        let stack = KernelStack::allocate_thread().map_err(|_| Error::Allocation)?;
        let mut context = crate::hal::context::ThreadContext::empty();
        context.prepare(stack.top(), entry, argument);
        Ok(Self::from_parts(
            ThreadIdentity {
                id,
                name: hyper::sync::InterruptSpinLock::new(ThreadNameSnapshot::new(name)?),
            },
            ThreadObject::try_system(id, ThreadRole::Kernel)?,
            placement,
            SchedulingPolicy::fair(),
            ThreadState::Dormant,
            Self::allocate_resources(context, Some(stack), ThreadExecution::Kernel, None)?,
        ))
    }

    /// Creates the permanent fallback Thread for one already-registered CPU.
    pub(super) fn idle(
        id: ThreadId,
        cpu_index: CpuIndex,
        entry: KernelThreadEntry,
    ) -> Result<Self, Error> {
        let stack = KernelStack::allocate_thread().map_err(|_| Error::Allocation)?;
        let mut context = crate::hal::context::ThreadContext::empty();
        context.prepare(stack.top(), entry, 0);
        Ok(Self::from_parts(
            ThreadIdentity {
                id,
                name: hyper::sync::InterruptSpinLock::new(ThreadNameSnapshot::per_cpu(
                    "idle", cpu_index,
                )?),
            },
            ThreadObject::try_system(id, ThreadRole::Idle)?,
            ThreadPlacement::pinned(cpu_index),
            SchedulingPolicy::Idle,
            ThreadState::Idle,
            Self::allocate_resources(context, Some(stack), ThreadExecution::Kernel, None)?,
        ))
    }

    /// Creates the already-running bootstrap context for a secondary CPU.
    pub(super) fn secondary_bootstrap(id: ThreadId, cpu_index: CpuIndex) -> Result<Self, Error> {
        Ok(Self::from_parts(
            ThreadIdentity {
                id,
                name: hyper::sync::InterruptSpinLock::new(ThreadNameSnapshot::per_cpu(
                    "idle", cpu_index,
                )?),
            },
            // A secondary bootstrap continuation exists solely to complete
            // local setup and become that CPU's permanent idle Thread.
            ThreadObject::try_system(id, ThreadRole::Idle)?,
            ThreadPlacement::pinned(cpu_index),
            SchedulingPolicy::fair(),
            ThreadState::Running,
            Self::allocate_resources(
                crate::hal::context::ThreadContext::empty(),
                Some(KernelStack::allocate_thread().map_err(|_| Error::Allocation)?),
                ThreadExecution::Kernel,
                None,
            )?,
        ))
    }

    pub(super) fn vcpu(
        id: ThreadId,
        cpu_index: CpuIndex,
        name: &str,
        execution: ExternalThreadExecution,
        mut ownership: ThreadResourceOwnership,
        entry: KernelThreadEntry,
    ) -> Result<Self, Error> {
        let stack = KernelStack::allocate_thread().map_err(|_| Error::Allocation)?;
        let mut scheduling_context = crate::hal::context::ThreadContext::empty();
        scheduling_context.prepare_vcpu(stack.top(), entry, 0);
        let placement =
            ThreadPlacement::movable_with_affinity(cpu_index, crate::task::policy::CpuMask::ALL)
                .ok_or(Error::InvalidPlacement)?;
        Ok(Self::from_parts(
            ThreadIdentity {
                id,
                name: hyper::sync::InterruptSpinLock::new(ThreadNameSnapshot::new(name)?),
            },
            ThreadObject::try_accounted_system(
                id,
                ThreadRole::Vcpu,
                ownership.take_object_charge(),
            )?,
            placement,
            SchedulingPolicy::Budget,
            ThreadState::Dormant,
            Self::allocate_resources(
                scheduling_context,
                Some(stack),
                ThreadExecution::Vcpu(execution),
                Some(ownership),
            )?,
        ))
    }

    pub(super) fn user(
        id: ThreadId,
        cpu_index: CpuIndex,
        affinity: crate::task::policy::CpuMask,
        name: &str,
        object: crate::process::UserThread,
        execution: Box<UnsafeCell<crate::process::UserExecution>>,
        entry: KernelThreadEntry,
    ) -> Result<Self, Error> {
        let stack = KernelStack::allocate_thread().map_err(|_| Error::Allocation)?;
        let mut context = crate::hal::context::ThreadContext::empty();
        context.prepare(stack.top(), entry, 0);
        let placement = ThreadPlacement::movable_with_affinity(cpu_index, affinity)
            .ok_or(Error::InvalidPlacement)?;
        Ok(Self::from_parts(
            ThreadIdentity {
                id,
                name: hyper::sync::InterruptSpinLock::new(ThreadNameSnapshot::new(name)?),
            },
            ThreadObject::user(object),
            placement,
            SchedulingPolicy::fair(),
            ThreadState::Dormant,
            Self::allocate_resources(context, Some(stack), ThreadExecution::User(execution), None)?,
        ))
    }

    pub const fn id(&self) -> ThreadId {
        self.identity.id
    }

    pub(crate) fn object_snapshot(&self) -> ThreadObjectSnapshot {
        self.object.snapshot()
    }

    pub(super) fn user_thread(&self) -> Option<&crate::process::UserThread> {
        self.object.user_thread()
    }

    pub(super) fn stored_schedule(&self) -> &ThreadScheduleState {
        if self.schedule_owner != ScheduleOwner::Coordinator {
            hyper::debug::invariant_failure("task::thread::stored_schedule invariant");
        }
        // SAFETY: coordinator access is serialized by TransitionLock, and a
        // Cpu owner is rejected before the cell is dereferenced.
        unsafe { &*self.schedule.get() }
    }

    fn stored_schedule_mut(&mut self) -> &mut ThreadScheduleState {
        if self.schedule_owner != ScheduleOwner::Coordinator {
            hyper::debug::invariant_failure("task::thread::stored_schedule_mut invariant");
        }
        self.schedule.get_mut()
    }

    pub(super) fn with_coordinator_schedule_mut<R>(
        &mut self,
        operation: impl FnOnce(&mut ThreadScheduleState) -> R,
    ) -> R {
        operation(self.stored_schedule_mut())
    }

    /// Transfers one coordinator-owned schedule into a CPU scheduling domain.
    pub(super) fn claim_schedule(&mut self, cpu: CpuIndex) -> bool {
        if self.schedule_owner != ScheduleOwner::Coordinator {
            return false;
        }
        let schedule = self.schedule.get_mut();
        if schedule.placement.assigned_cpu() != cpu
            || !matches!(
                schedule.state,
                ThreadState::Running | ThreadState::Idle | ThreadState::Blocked
            )
            || schedule.ready_queue_links.membership != QueueMembership::None
        {
            return false;
        }
        self.schedule_owner = ScheduleOwner::Cpu(cpu);
        true
    }

    /// Returns a CPU-owned schedule to the coordinator after it is absent
    /// from that CPU's current and ready ownership structures.
    pub(super) fn release_schedule(&mut self, cpu: CpuIndex) -> bool {
        if self.schedule_owner != ScheduleOwner::Cpu(cpu) {
            return false;
        }
        self.schedule_owner = ScheduleOwner::Coordinator;
        true
    }

    /// Accesses CPU-owned schedule state under the matching CPU lock.
    ///
    /// # Safety
    ///
    /// The caller must hold CPU `cpu`'s scheduler lock and must have
    /// revalidated `schedule_owner_cpu() == Some(cpu)` after acquiring it.
    pub(super) unsafe fn with_cpu_schedule<R>(
        &self,
        cpu: CpuIndex,
        operation: impl FnOnce(&ThreadScheduleState) -> R,
    ) -> Option<R> {
        if self.schedule_owner != ScheduleOwner::Cpu(cpu) {
            return None;
        }
        // SAFETY: guaranteed by the caller's matching CPU-lock authority.
        Some(operation(unsafe { &*self.schedule.get() }))
    }

    /// Mutably accesses CPU-owned schedule state under the matching CPU lock.
    ///
    /// # Safety
    ///
    /// The caller must hold CPU `cpu`'s scheduler lock exclusively and must
    /// have revalidated the owner locator after acquiring it.
    pub(super) unsafe fn with_cpu_schedule_mut<R>(
        &self,
        cpu: CpuIndex,
        operation: impl FnOnce(&mut ThreadScheduleState) -> R,
    ) -> Option<R> {
        if self.schedule_owner != ScheduleOwner::Cpu(cpu) {
            return None;
        }
        // SAFETY: guaranteed by the caller's matching CPU-lock authority.
        Some(operation(unsafe { &mut *self.schedule.get() }))
    }

    /// Commits coordinator-owned state to one ready queue.
    ///
    /// Queue code performs every fallible topology check before this operation;
    /// failure therefore indicates an internal transaction bug and must not be
    /// recovered after neighboring links have changed.
    pub(super) fn publish_ready_ownership(&mut self, cpu: CpuIndex, links: QueueLinks) -> bool {
        if links.membership
            != match links.membership {
                QueueMembership::ReadyRealTime { priority, .. } => {
                    QueueMembership::ReadyRealTime { cpu, priority }
                }
                QueueMembership::ReadyFair { .. } => QueueMembership::ReadyFair { cpu },
                _ => return false,
            }
        {
            return false;
        }
        if self.schedule_owner != ScheduleOwner::Coordinator {
            return false;
        }
        let schedule = self.schedule.get_mut();
        if schedule.ready_queue_links.membership != QueueMembership::None
            || self.control_queue_links.get_mut().membership != QueueMembership::None
        {
            return false;
        }
        schedule.ready_queue_links = links;
        schedule.state = ThreadState::Ready;
        self.schedule_owner = ScheduleOwner::Cpu(cpu);
        true
    }

    /// Returns the CPU that owns this thread's scheduling context.
    ///
    /// Assignment changes only through the scheduler's stopped-thread handoff.
    pub fn cpu_index(&self) -> CpuIndex {
        self.stored_schedule().placement.assigned_cpu()
    }

    pub(super) fn affinity(&self) -> crate::task::policy::CpuMask {
        self.stored_schedule().placement.affinity()
    }

    pub(super) fn placement_policy(&self) -> crate::task::policy::PlacementPolicy {
        self.stored_schedule().placement.policy()
    }

    pub(crate) fn name_snapshot(&self) -> ThreadNameSnapshot {
        self.identity.name.with(|name| *name)
    }

    /// Changes only the diagnostic label, never object identity or scheduling.
    pub(super) fn set_name(&self, name: &str) -> Result<(), Error> {
        let name = ThreadNameSnapshot::new(name)?;
        self.identity.name.with(|current| *current = name);
        Ok(())
    }

    pub fn state(&self) -> ThreadState {
        self.stored_schedule().state
    }

    pub(crate) fn scheduling_policy(&self) -> SchedulingPolicy {
        self.stored_schedule().scheduling
    }

    pub(crate) fn scheduling_class(&self) -> SchedulingClass {
        self.stored_schedule().scheduling.class()
    }

    pub fn priority(&self) -> Option<ThreadPriority> {
        self.stored_schedule().scheduling.priority()
    }

    pub(super) fn set_scheduling_policy(&mut self, policy: SchedulingPolicy) -> bool {
        if self.scheduling_class() == SchedulingClass::Idle {
            return false;
        }
        let schedule = self.stored_schedule_mut();
        schedule.install_parameters(super::scheduler::default_parameters(policy));
        true
    }

    pub(super) fn fair_slice_expired(&self) -> bool {
        let schedule = self.stored_schedule();
        schedule.fair_slice_expired()
    }

    pub(super) fn queue_links(&self) -> QueueLinks {
        // SAFETY: coordinator schedule access is legal only under the global
        // transition authority, which also owns the control-link cell.
        unsafe { self.combined_queue_links(self.stored_schedule().ready_queue_links) }
    }

    /// Combines disjoint ready and control topology under transition authority.
    ///
    /// # Safety
    ///
    /// The caller must hold the global `TransitionLock`. A CPU lock alone does
    /// not authorize reading the control-link cell.
    pub(super) unsafe fn combined_queue_links(&self, ready: QueueLinks) -> QueueLinks {
        // SAFETY: inherited from this method's caller contract.
        let control = unsafe { self.control_queue_links() };
        match (ready.membership, control.membership) {
            (QueueMembership::None, _) => control,
            (_, QueueMembership::None) => ready,
            _ => hyper::debug::invariant_failure("task::thread::combined_queue_links invariant"),
        }
    }

    pub(super) fn table_owned_ready_queue_links(&self) -> Option<QueueLinks> {
        match self.schedule_owner {
            ScheduleOwner::Coordinator => Some(self.stored_schedule().ready_queue_links),
            ScheduleOwner::Cpu(_) => None,
        }
    }

    /// Returns links under exclusive coordination or matching queue authority.
    pub(super) unsafe fn control_queue_links(&self) -> QueueLinks {
        // SAFETY: the caller owns exclusive coordination or the matching
        // queue authority. The cell is disjoint from CPU schedule storage.
        unsafe { *self.control_queue_links.get() }
    }

    /// Mutates transition-coordinator queue topology without borrowing a
    /// CPU-owned schedule.
    ///
    /// # Safety
    ///
    /// The caller must hold exclusive coordination, or the affected queue
    /// lock and a registry reader lane. An unlinked candidate also requires
    /// its CPU lock. Other referenced nodes must belong to the locked queue.
    pub(super) unsafe fn with_control_queue_links_mut<R>(
        &self,
        operation: impl FnOnce(&mut QueueLinks) -> R,
    ) -> R {
        // SAFETY: guaranteed by the caller's linear control authority.
        operation(unsafe { &mut *self.control_queue_links.get() })
    }

    pub(super) fn wait_context(&self) -> &super::ThreadWaitContext {
        &self.wait_context
    }

    pub(super) fn wait_record(&self) -> &WaitRecord {
        &self.stored_schedule().wait
    }

    pub(super) fn pending_migration(&self) -> Option<MigrationRequest> {
        self.stored_schedule().pending_migration
    }

    /// Returns the stable machine-context address without minting a Rust borrow.
    ///
    /// The scheduler's switch protocol exclusively owns mutation from switch
    /// preparation until the incoming tail retires `switching_from`.
    pub(super) fn context_pointer(&self) -> *mut crate::hal::context::ThreadContext {
        self.resources.context.get()
    }

    pub fn execution_kind(&self) -> ExecutionKind {
        match self.resources.execution {
            ThreadExecution::Kernel => ExecutionKind::Kernel,
            ThreadExecution::Vcpu(_) => ExecutionKind::Vcpu,
            ThreadExecution::User(_) => ExecutionKind::User,
        }
    }

    /// Returns the stable vCPU payload address without creating `&mut`.
    ///
    /// Current-vCPU admission and hardware ownership serialize all dereference
    /// of this pointer. Repeated scheduler queries therefore cannot invalidate
    /// a previously issued raw capability by retagging an exclusive reference.
    pub(super) fn vcpu_execution_pointer(
        &self,
    ) -> Option<super::external_execution::ExternalExecutionPointer> {
        match &self.resources.execution {
            ThreadExecution::Vcpu(execution) => Some(execution.pointer()),
            _ => None,
        }
    }

    /// Returns the stable user payload address through a shared reference.
    ///
    /// `UserExecution` confines machine-register mutation to its own
    /// `UnsafeCell`; scheduler identity and lifecycle observation are shared.
    pub(super) fn user_execution_pointer(
        &self,
    ) -> Option<core::ptr::NonNull<crate::process::UserExecution>> {
        match &self.resources.execution {
            ThreadExecution::User(execution) => core::ptr::NonNull::new(execution.get()),
            _ => None,
        }
    }

    /// Arms a dormant user payload before its first scheduler publication.
    pub(super) fn arm_user_execution(
        &mut self,
        ownership: crate::process::UserExecutionOwnership,
    ) -> bool {
        match &mut self.resources.execution {
            ThreadExecution::User(execution) => {
                execution.get_mut().arm_for_process_publication(ownership);
                true
            }
            _ => false,
        }
    }

    /// Extracts user ownership only after the scheduler proved context stop.
    pub(super) fn take_user_execution(
        &mut self,
    ) -> Option<(
        crate::process::UserThread,
        Box<UnsafeCell<crate::process::UserExecution>>,
    )> {
        if !matches!(self.resources.execution, ThreadExecution::User(_)) {
            return None;
        }
        let object = match self.object.user_thread() {
            Some(thread) => thread.clone(),
            None => hyper::debug::invariant_failure("task::thread::take_user_execution invariant"),
        };
        match core::mem::replace(&mut self.resources.execution, ThreadExecution::Kernel) {
            ThreadExecution::User(execution) => Some((object, execution)),
            _ => hyper::debug::invariant_failure("task::thread::take_user_execution invariant"),
        }
    }

    pub(super) fn take_vcpu_reap_publication(&mut self) -> Option<ThreadRetirement> {
        let mut retirement = match &mut self.resources.execution {
            // `detach_terminated` proved no CPU or switch tail owns this
            // payload, so the cell's unique owner may safely use `get_mut`.
            ThreadExecution::Vcpu(execution) => execution.take_retirement(),
            _ => None,
        }?;
        let ownership = match self.resources._ownership.as_mut() {
            Some(ownership) => ownership,
            None => hyper::debug::invariant_failure(
                "task::thread::take_vcpu_reap_publication invariant",
            ),
        };
        retirement.retain_charge(ownership.take_retirement_charge());
        Some(retirement)
    }

    pub const fn owns_kernel_stack(&self) -> bool {
        self.resources.kernel_stack.is_some()
    }

    pub(super) fn ensure_kernel_stack(&mut self) -> Result<(usize, usize), Error> {
        if self.resources.kernel_stack.is_none() {
            self.resources.kernel_stack =
                Some(KernelStack::allocate_thread().map_err(|_| Error::Allocation)?);
        }
        self.kernel_stack_bounds().ok_or(Error::Allocation)
    }

    pub(super) fn kernel_stack_top(&self) -> Option<usize> {
        self.resources.kernel_stack.as_ref().map(KernelStack::top)
    }

    pub(super) fn kernel_stack_physical_top(&self) -> Option<u64> {
        self.resources
            .kernel_stack
            .as_ref()
            .map(KernelStack::physical_top)
    }

    pub(super) fn kernel_stack_bounds(&self) -> Option<(usize, usize)> {
        self.resources
            .kernel_stack
            .as_ref()
            .map(KernelStack::bounds)
    }

    pub(super) fn kernel_stack_statistics(&self) -> Option<crate::mm::stack::StackStatistics> {
        self.resources
            .kernel_stack
            .as_ref()
            .map(KernelStack::statistics)
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub struct ThreadNameSnapshot {
    bytes: [u8; MAX_THREAD_NAME_BYTES],
    len: u8,
}

impl ThreadNameSnapshot {
    /// Formats a CPU-owned thread identity without a temporary heap allocation.
    pub(crate) fn per_cpu(prefix: &str, cpu: CpuIndex) -> Result<Self, Error> {
        let mut name = Self::new(prefix)?;
        let separator = name
            .bytes
            .get_mut(usize::from(name.len))
            .ok_or(Error::NameTooLong)?;
        *separator = b'/';
        name.len += 1;
        let start = usize::from(name.len);
        let mut number = cpu.get();
        loop {
            let index = usize::from(name.len);
            let slot = name.bytes.get_mut(index).ok_or(Error::NameTooLong)?;
            *slot = b'0' + (number % 10) as u8;
            name.len += 1;
            number /= 10;
            if number == 0 {
                break;
            }
        }
        name.bytes[start..usize::from(name.len)].reverse();
        Ok(name)
    }

    fn new(name: &str) -> Result<Self, Error> {
        if name.len() > MAX_THREAD_NAME_BYTES {
            return Err(Error::NameTooLong);
        }
        let mut bytes = [0; MAX_THREAD_NAME_BYTES];
        bytes[..name.len()].copy_from_slice(name.as_bytes());
        Ok(Self {
            bytes,
            len: name.len() as u8,
        })
    }

    pub fn as_str(&self) -> &str {
        let bytes = &self.bytes[..usize::from(self.len)];
        // Snapshots are built from UTF-8 input. Keep the accessor defensive if
        // a future internal constructor violates that invariant.
        core::str::from_utf8(bytes).unwrap_or("")
    }

    pub(crate) fn as_bytes(&self) -> &[u8] {
        &self.bytes[..usize::from(self.len)]
    }
}

impl core::fmt::Debug for ThreadNameSnapshot {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        self.as_str().fmt(formatter)
    }
}

impl core::fmt::Display for ThreadNameSnapshot {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter.write_str(self.as_str())
    }
}
