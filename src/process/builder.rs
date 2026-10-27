// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Transactional construction policy for Native userspace processes.
//!
//! A builder owns the authority and allocation work needed to construct one
//! child. It deliberately does not expose the partially constructed Process.
//! The syscall layer can therefore publish supervisor Process and startup-channel
//! handles at the same commit that makes the initial Thread runnable.

use crate::capability::InvocationReceiver;

use alloc::{string::String, vec::Vec};
use core::sync::atomic::{AtomicBool, Ordering};

use crate::authority::Rights;
use crate::mm::user_space::NativeAddressSpace;
use crate::task::scheduler::CpuMask;
use hyper::sync::InterruptSpinLock;
use {
    crate::accounting::CommittedCharge, crate::accounting::ResourceAmount,
    crate::accounting::ResourceDomain, crate::accounting::ResourceDomainObject,
    crate::accounting::ResourceError, crate::accounting::ResourceKind,
};
use {
    crate::capability::HandleFlags, crate::capability::HandleInfo,
    crate::capability::HandleTransferOperation, crate::capability::HandleTransferRequest,
    crate::capability::HandleTransferRoute, crate::capability::HandleValue,
    crate::capability::InTransitCapabilities, crate::capability::PreparedHandle,
};
use {crate::fs::vfs::ExecutableSnapshot, crate::fs::vfs::FileObject, crate::fs::vfs::VfsError};
use {
    crate::object::KernelObject, crate::object::ObjectCreationError, crate::object::ObjectKind,
    crate::object::ObjectPublication, crate::object::TransferClass,
    crate::object::object_allocation_size, crate::object::private,
};

use super::builder_input::{
    ABI_AFFINITY_WORDS, MAX_STARTUP_DATA_BYTES, MAX_STARTUP_HANDLES, valid_name,
};
use super::builder_policy::BuilderStorable;
use super::{
    LoaderError, PreparedProcess, Process, TaskFactory, TaskGroup, TaskGroupObject, load_native,
};
pub(super) const EXECUTABLE_RIGHTS: Rights =
    Rights::READ.union(Rights::EXECUTE).union(Rights::PROTECT);

const _: () = assert!(hyper::cpu::MAX_CPUS <= ABI_AFFINITY_WORDS * u64::BITS as usize);
const _: () = assert!(MAX_STARTUP_HANDLES > 5);
const MAX_USER_STARTUP_HANDLES: usize = MAX_STARTUP_HANDLES - 5;

/// Failure before the atomic child-publication commit.
#[derive(Debug)]
pub(crate) enum ProcessBuilderError<E> {
    Allocation,
    AlreadySealed,
    AlreadyStarted,
    Aborted,
    ExecutableFile(VfsError),
    Busy,
    DuplicateStartupPurpose,
    Image(LoaderError),
    InvalidAffinity,
    InvalidScheduling,
    InvalidStartupPurpose,
    InvalidString,
    MissingName,
    NotSealed,
    Object(ObjectCreationError),
    Process(super::ProcessError),
    Resource(ResourceError),
    StartupHandleLimit,
    DataLimit,
    Transaction(E),
    UnsupportedStartupKind,
    Retention(super::retention::Error),
}

/// One process-local handle requested for the child's startup record array.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct StartupCapability {
    purpose: u32,
    value: HandleValue,
    requested_rights: Option<crate::authority::Rights>,
    expected_kind: Option<crate::object::ObjectKind>,
    operation: HandleTransferOperation,
}

impl StartupCapability {
    pub(crate) const fn new(
        purpose: u32,
        value: HandleValue,
        requested_rights: Option<crate::authority::Rights>,
        expected_kind: Option<crate::object::ObjectKind>,
        operation: HandleTransferOperation,
    ) -> Self {
        Self {
            purpose,
            value,
            requested_rights,
            expected_kind,
            operation,
        }
    }

    const fn transfer_request(self, rights: crate::authority::Rights) -> HandleTransferRequest {
        HandleTransferRequest {
            value: self.value,
            offered_rights: None,
            rights,
            offered_kind: None,
            expected_kind: self.expected_kind,
            operation: self.operation,
        }
    }
}

struct BuilderPlan {
    // These canonical owners are captured only after the syscall layer has
    // resolved the corresponding typed handles and rights. The builder does
    // not retain an OperationPin beyond that operation or redo a namespace
    // lookup after construction.
    group: TaskGroup,
    domain: ResourceDomain,
    executable: PreparedHandle,
    loader: Option<ExecutableSnapshot>,
    data: Vec<u8>,
    thread_name: Option<String>,
    affinity: CpuMask,
    memory_lock_allowed: bool,
    scheduling: super::scheduling::ProcessScheduling,
    retention: Option<hyper::mm::FallibleArc<super::retention::Retention>>,
    startup: Vec<StoredStartupCapability>,
}

impl Drop for BuilderPlan {
    fn drop(&mut self) {
        if let Some(retention) = self.retention.take() {
            retention.release(super::retention::RELEASE_ABORTED);
            retention.discard_released();
        }
    }
}

/// Authority already detached from its source namespace for child startup.
///
/// A staged entry contains exactly one active capability owner. The wrapper's
/// explicit drop path makes an abandoned builder close that authority outside
/// the builder state lock instead of relying on an ambient source Process.
pub(super) struct StoredStartupCapability {
    purpose: u32,
    authority: Option<InTransitCapabilities>,
}

impl StoredStartupCapability {
    fn new(purpose: u32, authority: InTransitCapabilities) -> Self {
        if authority.len() != 1 {
            builder_invariant_violation();
        }
        Self {
            purpose,
            authority: Some(authority),
        }
    }

    pub(super) const fn purpose(&self) -> u32 {
        self.purpose
    }

    /// Consumes the staged entry at the child-publication commit.
    pub(super) fn take_authority(mut self) -> InTransitCapabilities {
        match self.authority.take() {
            Some(authority) => authority,
            None => builder_invariant_violation(),
        }
    }

    fn release_into(&mut self, retirement: &mut crate::object::ObjectRetirement) {
        if let Some(authority) = self.authority.take() {
            authority.release_into(retirement);
        }
    }
}

impl Drop for StoredStartupCapability {
    fn drop(&mut self) {
        let mut retirement = crate::object::ObjectRetirement::new();
        self.release_into(&mut retirement);
        retirement.drain();
    }
}

pub(super) struct SealedProcessBuild {
    child: Option<PreparedProcess>,
    plan: BuilderPlan,
}

impl SealedProcessBuild {
    pub(super) fn child(&self) -> &PreparedProcess {
        match self.child.as_ref() {
            Some(child) => child,
            None => builder_invariant_violation(),
        }
    }

    pub(super) fn data(&self) -> &[u8] {
        &self.plan.data
    }
    pub(super) fn executable(&self) -> &PreparedHandle {
        &self.plan.executable
    }

    pub(super) fn thread_name(&self) -> &str {
        match self.plan.thread_name.as_deref() {
            Some(name) => name,
            None => builder_invariant_violation(),
        }
    }

    pub(super) const fn affinity(&self) -> CpuMask {
        self.plan.affinity
    }

    pub(super) const fn memory_lock_allowed(&self) -> bool {
        self.plan.memory_lock_allowed
    }

    pub(super) fn startup_capabilities(&self) -> &[StoredStartupCapability] {
        &self.plan.startup
    }

    /// Returns every linear owner to the process subsystem for commit.
    pub(super) fn into_parts(mut self) -> (PreparedProcess, Vec<StoredStartupCapability>) {
        let child = match self.child.take() {
            Some(child) => child,
            None => builder_invariant_violation(),
        };
        // The published child, not the consumed builder plan, now owns cleanup.
        self.plan.retention.take();
        (child, core::mem::take(&mut self.plan.startup))
    }

    fn abort(mut self) {
        self.abort_inner();
    }

    fn abort_inner(&mut self) {
        if let Some(child) = self.child.take() {
            retire_unpublished_address_space(child.abort());
        }
    }
}

impl Drop for SealedProcessBuild {
    fn drop(&mut self) {
        self.abort_inner();
    }
}

enum BuilderState {
    Open(BuilderPlan),
    Sealed(SealedProcessBuild),
    Busy,
    /// Abort-vs-start arbitration has committed; publication is inevitable.
    CommitAuthorized,
    Started,
    Aborted,
}

/// Backend contract for the single child-publication commit.
///
/// `prepare_start` must perform every fallible operation: destination
/// reservations, bootstrap-message preparation, dormant Thread construction,
/// scheduler admission, builder-handle consumption preparation, and
/// supervisor/result-channel handle reservation. On failure it returns the complete
/// `SealedProcessBuild`, including every builder-owned startup authority, while
/// the child remains invisible.
///
/// `commit_start` is the sole publication point and must be infallible. It
/// publishes the child Process, installs startup handles, returns the
/// supervisor Process and startup result channel to the parent, and makes the initial Thread
/// runnable in that order under the transaction's serialization contract. It
/// runs only after abort-vs-start arbitration and never under the builder state
/// lock. Destruction and zero-handle callbacks remain deferred to
/// `finish_start`.
pub(super) trait ProcessStartTransaction {
    type Error;
    type Prepared;
    type Committed;
    type Output;

    #[expect(
        clippy::result_large_err,
        reason = "the error must retain the complete linear build owner for allocation-free rollback"
    )]
    fn prepare_start(
        &mut self,
        build: SealedProcessBuild,
    ) -> Result<Self::Prepared, StartPreparationFailure<Self::Error>>;

    fn commit_start(&mut self, prepared: Self::Prepared) -> Self::Committed;

    /// Releases detached authority and transaction storage after the builder
    /// state lock has published `Started`.
    fn finish_start(&mut self, committed: Self::Committed) -> Self::Output;

    /// Rolls every prepared reservation back without allocation or failure.
    fn cancel_start(&mut self, prepared: Self::Prepared) -> SealedProcessBuild;
}

pub(super) struct StartPreparationFailure<E> {
    pub(super) error: E,
    pub(super) build: SealedProcessBuild,
}

/// Linear owner of one not-yet-visible child construction.
pub(crate) struct ProcessBuilder {
    state: InterruptSpinLock<Option<BuilderState>, crate::hal::irq::LocalMask>,
    abort_requested: AtomicBool,
    _object_charge: CommittedCharge,
}

impl ProcessBuilder {
    const BASE_RIGHTS: Rights = Rights::TRANSFER
        .union(Rights::SCHED_LOWER)
        .union(Rights::INSPECT)
        .union(Rights::WRITE)
        .union(Rights::START)
        .union(Rights::REQUEST_STOP);

    pub(crate) fn try_publication(
        _factory: &TaskFactory,
        group: &TaskGroupObject,
        domain: &ResourceDomainObject,
        executable: PreparedHandle,
        loader: &FileObject,
    ) -> Result<ObjectPublication<Self>, ProcessBuilderError<()>> {
        let loader = loader
            .executable_snapshot(domain.domain())
            .map_err(ProcessBuilderError::ExecutableFile)?;
        let object_charge = reserve_builder_charge(domain.domain())?;
        let mut data = Vec::new();
        data.try_reserve_exact(MAX_STARTUP_DATA_BYTES)
            .map_err(|_| ProcessBuilderError::Allocation)?;
        let mut startup = Vec::new();
        startup
            .try_reserve_exact(MAX_USER_STARTUP_HANDLES)
            .map_err(|_| ProcessBuilderError::Allocation)?;
        let builder = Self {
            state: InterruptSpinLock::new(Some(BuilderState::Open(BuilderPlan {
                group: group.group().clone(),
                domain: domain.domain().clone(),
                executable,
                loader: Some(loader),
                data,
                thread_name: None,
                affinity: CpuMask::ALL,
                memory_lock_allowed: false,
                scheduling: super::scheduling::ProcessScheduling::default_native(),
                retention: None,
                startup,
            }))),
            abort_requested: AtomicBool::new(false),
            _object_charge: object_charge,
        };
        ObjectPublication::try_new(builder).map_err(ProcessBuilderError::Object)
    }

    pub(crate) fn add_startup_capability(
        &self,
        source: &Process,
        capability: StartupCapability,
    ) -> Result<(), ProcessBuilderError<()>> {
        let mut plan = self.take_open_plan()?;
        let info = match validate_startup_capability(&mut plan, capability, source) {
            Ok(info) => info,
            Err(error) => return self.finish_open_operation(plan, Err(error)),
        };
        let requested_rights = capability.requested_rights.unwrap_or(info.rights);
        let request = capability.transfer_request(requested_rights);
        let transfer = match source.prepare_handle_transfer(
            core::slice::from_ref(&request),
            None,
            Some(ObjectKind::PROCESS_BUILDER),
            HandleTransferRoute::StagedStartup,
        ) {
            Ok(transfer) => transfer,
            Err(error) => {
                return self.finish_open_operation(plan, Err(ProcessBuilderError::Process(error)));
            }
        };
        let authority = match transfer.commit() {
            Ok(authority) => authority,
            Err(failure) => {
                let error = failure.error;
                failure.transfer.rollback();
                return self.finish_open_operation(plan, Err(ProcessBuilderError::Process(error)));
            }
        };
        // Capacity was reserved before the source transaction. From this point
        // onward insertion cannot fail, so successful MOVE ownership is never
        // reported to userspace as a recoverable failure.
        plan.startup
            .push(StoredStartupCapability::new(capability.purpose, authority));
        self.finish_committed_add(plan)
    }

    pub(crate) fn set_name(&self, name: &str) -> Result<(), ProcessBuilderError<()>> {
        let mut plan = self.take_open_plan()?;
        let result = copy_name(name).map(|name| {
            plan.thread_name = Some(name);
        });
        self.finish_open_operation(plan, result)
    }

    /// Arms retention before the child exists. The creator receives the only
    /// cleanup controller; staged startup does not admit that object kind.
    pub(crate) fn retain_on_exit(
        &self,
        owner: &Process,
        max_bytes: u64,
        timeout_ns: u64,
    ) -> Result<PreparedHandle, ProcessBuilderError<()>> {
        let mut plan = self.take_open_plan()?;
        if plan.retention.is_some() {
            return self.finish_open_operation(plan, Err(ProcessBuilderError::Busy));
        }
        let result = (|| {
            let retention = super::retention::Retention::new(owner, max_bytes, timeout_ns)
                .map_err(ProcessBuilderError::Retention)?;
            let controller = super::retention::ProcessRetirement::new(
                retention.clone(),
                &owner.resource_domain(),
            )
            .map_err(ProcessBuilderError::Retention)?;
            // Prepare every fallible object/registry allocation while the
            // builder plan is exclusively owned. A live caller can retry any
            // failure without leaving a silently revoked retention policy.
            let object =
                ObjectPublication::try_new(controller).map_err(ProcessBuilderError::Object)?;
            let prepared = PreparedHandle::try_from_new_object(
                object,
                super::retention::ProcessRetirement::RIGHTS,
                HandleFlags::NONE,
            )
            .map_err(|error| ProcessBuilderError::Process(error.into()))?;
            plan.retention = Some(retention);
            Ok(prepared)
        })();
        self.finish_open_operation(plan, result)
    }

    /// Replaces a bounded opaque userspace payload. The kernel does not
    /// interpret arguments, environment entries, or the runtime's wire format.
    pub(crate) fn set_data(&self, data: &[u8]) -> Result<(), ProcessBuilderError<()>> {
        let mut plan = self.take_open_plan()?;
        if data.len() > MAX_STARTUP_DATA_BYTES {
            return self.finish_open_operation(plan, Err(ProcessBuilderError::DataLimit));
        }
        plan.data.clear();
        plan.data.extend_from_slice(data);
        self.finish_open_operation(plan, Ok(()))
    }

    pub(crate) fn set_affinity(&self, affinity: CpuMask) -> Result<(), ProcessBuilderError<()>> {
        let mut plan = self.take_open_plan()?;
        if crate::task::scheduler::validate_affinity(affinity).is_err() {
            return self.finish_open_operation(plan, Err(ProcessBuilderError::InvalidAffinity));
        }
        plan.affinity = affinity;
        self.finish_open_operation(plan, Ok(()))
    }

    /// Validates all scheduling fields and child authority before committing either.
    pub(crate) fn set_scheduling(
        &self,
        parameters: crate::task::policy::SchedulingParameters,
        authority: crate::task::scheduling_authority::SchedulingAuthority,
        delegated: Rights,
    ) -> Result<(), ProcessBuilderError<()>> {
        let mut plan = self.take_open_plan()?;
        let valid = authority
            .validate_delegation(plan.scheduling.delegated, delegated)
            .is_ok()
            && authority
                .validate_change(plan.scheduling.initial, parameters, plan.scheduling.limits)
                .is_ok();
        if !valid {
            return self.finish_open_operation(
                plan,
                Err(ProcessBuilderError::Process(
                    crate::capability::HandleError::AccessDenied.into(),
                )),
            );
        }
        if crate::task::scheduler::validate_scheduling_parameters(parameters).is_err() {
            return self.finish_open_operation(plan, Err(ProcessBuilderError::InvalidScheduling));
        }
        plan.scheduling.initial = parameters;
        plan.scheduling.delegated = delegated.union(Rights::SCHED_LOWER);
        self.finish_open_operation(plan, Ok(()))
    }

    fn inherit_scheduling(
        &self,
        parameters: crate::task::policy::SchedulingParameters,
        rights: Rights,
    ) -> Result<(), ProcessBuilderError<()>> {
        use {
            crate::task::scheduling_authority::SCHEDULING_RIGHTS,
            crate::task::scheduling_authority::SchedulingLimits,
        };
        let mut plan = self.take_open_plan()?;
        let elevated =
            rights.intersection(SCHEDULING_RIGHTS).bits() & !Rights::SCHED_LOWER.bits() != 0;
        plan.scheduling = super::scheduling::ProcessScheduling {
            initial: parameters,
            limits: if elevated {
                SchedulingLimits::SYSTEM
            } else {
                SchedulingLimits::inherited(parameters)
            },
            delegated: Rights::SCHED_LOWER,
        };
        self.finish_open_operation(plan, Ok(()))
    }

    /// Explicitly enables residency authority on the child's root VMAR.
    /// The syscall adapter must resolve the builder with `LOCK_MEMORY` first.
    pub(crate) fn set_memory_lock(&self, enabled: bool) -> Result<(), ProcessBuilderError<()>> {
        let mut plan = self.take_open_plan()?;
        plan.memory_lock_allowed = enabled;
        self.finish_open_operation(plan, Ok(()))
    }

    /// Prepares the bootstrap address space while the child remains unpublished.
    ///
    /// Every error leaves the builder Open and reusable. Sealing claims no
    /// source handles, creates no Thread and publishes no Process identity.
    pub(crate) fn seal(&self) -> Result<(), ProcessBuilderError<()>> {
        let mut plan = self.take_open_plan()?;
        let prepared = match prepare_sealed_process(&plan) {
            Ok(prepared) => prepared,
            Err(error) => return self.finish_open_operation(plan, Err(error)),
        };
        // Installed loader pages own independent storage. No full-image snapshot
        // survives sealing, even if the caller leaves the builder dormant.
        drop(plan.loader.take());
        let mut sealed = Some(SealedProcessBuild {
            child: Some(prepared),
            plan,
        });
        let aborted = self.state.with(|state| {
            require_busy_state(state);
            if self.abort_requested.load(Ordering::Acquire) {
                *state = Some(BuilderState::Aborted);
                true
            } else {
                *state = Some(BuilderState::Sealed(match sealed.take() {
                    Some(sealed) => sealed,
                    None => builder_invariant_violation(),
                }));
                false
            }
        });
        if aborted {
            drop(sealed.take());
            return Err(ProcessBuilderError::Aborted);
        }
        Ok(())
    }

    /// Executes one prevalidated, allocation-free publication commit.
    ///
    /// The transaction remains locally owned across abort arbitration and the
    /// single publication commit.
    fn start_with<T: ProcessStartTransaction>(
        &self,
        transaction: &mut T,
    ) -> Result<T::Output, ProcessBuilderError<T::Error>> {
        let build = self.take_sealed_build()?;
        let prepared = match transaction.prepare_start(build) {
            Ok(prepared) => prepared,
            Err(failure) => {
                let mut build = Some(failure.build);
                let result = self.state.with(|state| {
                    require_busy_state(state);
                    if self.abort_requested.load(Ordering::Acquire) {
                        *state = Some(BuilderState::Aborted);
                        Err(ProcessBuilderError::Aborted)
                    } else {
                        *state = Some(BuilderState::Sealed(match build.take() {
                            Some(build) => build,
                            None => builder_invariant_violation(),
                        }));
                        Err(ProcessBuilderError::Transaction(failure.error))
                    }
                });
                drop(build.take());
                return result;
            }
        };
        let authorized = self.state.with(|state| {
            require_busy_state(state);
            if self.abort_requested.load(Ordering::Acquire) {
                *state = Some(BuilderState::Aborted);
                return false;
            }
            *state = Some(BuilderState::CommitAuthorized);
            true
        });
        if !authorized {
            transaction.cancel_start(prepared).abort();
            return Err(ProcessBuilderError::Aborted);
        }
        let committed = transaction.commit_start(prepared);
        self.state.with(|state| {
            require_commit_authorized_state(state);
            *state = Some(BuilderState::Started);
        });
        Ok(transaction.finish_start(committed))
    }

    pub(crate) fn abort(&self) -> Result<(), ProcessBuilderError<()>> {
        let mut abandoned_plan = None;
        let mut retired = None;
        let result = self.state.with(|state| {
            let current = match state.take() {
                Some(current) => current,
                None => builder_invariant_violation(),
            };
            match current {
                BuilderState::Open(plan) => {
                    self.abort_requested.store(true, Ordering::Release);
                    abandoned_plan = Some(plan);
                    *state = Some(BuilderState::Aborted);
                    Ok(())
                }
                BuilderState::Sealed(build) => {
                    self.abort_requested.store(true, Ordering::Release);
                    retired = Some(build);
                    *state = Some(BuilderState::Aborted);
                    Ok(())
                }
                BuilderState::Busy => {
                    self.abort_requested.store(true, Ordering::Release);
                    *state = Some(BuilderState::Busy);
                    Ok(())
                }
                BuilderState::CommitAuthorized => {
                    *state = Some(BuilderState::CommitAuthorized);
                    Err(ProcessBuilderError::AlreadyStarted)
                }
                BuilderState::Started => {
                    *state = Some(BuilderState::Started);
                    Err(ProcessBuilderError::AlreadyStarted)
                }
                BuilderState::Aborted => {
                    self.abort_requested.store(true, Ordering::Release);
                    *state = Some(BuilderState::Aborted);
                    Err(ProcessBuilderError::Aborted)
                }
            }
        });
        drop(abandoned_plan.take());
        drop(retired.take());
        result
    }

    fn take_open_plan(&self) -> Result<BuilderPlan, ProcessBuilderError<()>> {
        self.state.with(|state| {
            if self.abort_requested.load(Ordering::Acquire) {
                return Err(ProcessBuilderError::Aborted);
            }
            let current = match state.take() {
                Some(current) => current,
                None => builder_invariant_violation(),
            };
            match current {
                BuilderState::Open(plan) => {
                    *state = Some(BuilderState::Busy);
                    Ok(plan)
                }
                BuilderState::Sealed(build) => {
                    *state = Some(BuilderState::Sealed(build));
                    Err(ProcessBuilderError::AlreadySealed)
                }
                BuilderState::Busy => {
                    *state = Some(BuilderState::Busy);
                    Err(ProcessBuilderError::Busy)
                }
                BuilderState::CommitAuthorized => {
                    *state = Some(BuilderState::CommitAuthorized);
                    Err(ProcessBuilderError::AlreadyStarted)
                }
                BuilderState::Started => {
                    *state = Some(BuilderState::Started);
                    Err(ProcessBuilderError::AlreadyStarted)
                }
                BuilderState::Aborted => {
                    *state = Some(BuilderState::Aborted);
                    Err(ProcessBuilderError::Aborted)
                }
            }
        })
    }

    fn finish_open_operation<T>(
        &self,
        plan: BuilderPlan,
        result: Result<T, ProcessBuilderError<()>>,
    ) -> Result<T, ProcessBuilderError<()>> {
        let mut plan = Some(plan);
        let aborted = self.state.with(|state| {
            require_busy_state(state);
            if self.abort_requested.load(Ordering::Acquire) {
                *state = Some(BuilderState::Aborted);
                return true;
            }
            *state = Some(BuilderState::Open(match plan.take() {
                Some(plan) => plan,
                None => builder_invariant_violation(),
            }));
            false
        });
        drop(plan.take());
        if aborted {
            Err(ProcessBuilderError::Aborted)
        } else {
            result
        }
    }

    /// Completes an add whose source-handle transfer has already committed.
    ///
    /// A concurrent final close or explicit abort orders after that commit:
    /// both operations succeed and the abort releases the newly contained
    /// authority. Returning `Aborted` here would falsely promise that a MOVE
    /// source remained unchanged.
    fn finish_committed_add(&self, plan: BuilderPlan) -> Result<(), ProcessBuilderError<()>> {
        let mut plan = Some(plan);
        let aborted = self.state.with(|state| {
            require_busy_state(state);
            if self.abort_requested.load(Ordering::Acquire) {
                *state = Some(BuilderState::Aborted);
                true
            } else {
                *state = Some(BuilderState::Open(match plan.take() {
                    Some(plan) => plan,
                    None => builder_invariant_violation(),
                }));
                false
            }
        });
        if aborted {
            drop(plan.take());
        }
        Ok(())
    }

    fn take_sealed_build<E>(&self) -> Result<SealedProcessBuild, ProcessBuilderError<E>> {
        self.state.with(|state| {
            if self.abort_requested.load(Ordering::Acquire) {
                return Err(ProcessBuilderError::Aborted);
            }
            let current = match state.take() {
                Some(current) => current,
                None => builder_invariant_violation(),
            };
            match current {
                BuilderState::Sealed(build) => {
                    *state = Some(BuilderState::Busy);
                    Ok(build)
                }
                BuilderState::Open(plan) => {
                    *state = Some(BuilderState::Open(plan));
                    Err(ProcessBuilderError::NotSealed)
                }
                BuilderState::Busy => {
                    *state = Some(BuilderState::Busy);
                    Err(ProcessBuilderError::Busy)
                }
                BuilderState::CommitAuthorized => {
                    *state = Some(BuilderState::CommitAuthorized);
                    Err(ProcessBuilderError::AlreadyStarted)
                }
                BuilderState::Started => {
                    *state = Some(BuilderState::Started);
                    Err(ProcessBuilderError::AlreadyStarted)
                }
                BuilderState::Aborted => {
                    *state = Some(BuilderState::Aborted);
                    Err(ProcessBuilderError::Aborted)
                }
            }
        })
    }
}

/// Copies only the authority needed by the child's ELF loader. The normal
/// capability transfer transaction checks READ, EXECUTE, PROTECT, DUPLICATE and
/// TRANSFER together; EXECUTE alone never becomes readable child authority.
fn copy_executable_for_child(
    caller: &Process,
    executable: HandleValue,
) -> Result<PreparedHandle, ProcessBuilderError<()>> {
    let request = HandleTransferRequest {
        value: executable,
        offered_rights: None,
        rights: EXECUTABLE_RIGHTS,
        offered_kind: None,
        expected_kind: Some(ObjectKind::FILE),
        operation: HandleTransferOperation::Copy,
    };
    let transfer = caller
        .prepare_handle_transfer(
            core::slice::from_ref(&request),
            None,
            Some(ObjectKind::PROCESS_BUILDER),
            HandleTransferRoute::StagedStartup,
        )
        .map_err(ProcessBuilderError::Process)?;
    let authority = match transfer.commit() {
        Ok(authority) => authority,
        Err(failure) => {
            let error = failure.error;
            failure.transfer.rollback();
            return Err(ProcessBuilderError::Process(error));
        }
    };
    let (mut handles, charge) = authority.into_prepared_handles();
    let executable = handles
        .pop()
        .unwrap_or_else(|| builder_invariant_violation());
    drop(handles);
    drop(charge);
    Ok(executable)
}

/// Creates and publishes one builder from independently checked authorities.
pub(crate) fn create_process_builder(
    caller: &Process,
    inherited_scheduling: crate::task::policy::SchedulingParameters,
    factory: &InvocationReceiver,
    group: HandleValue,
    domain: HandleValue,
    executable: HandleValue,
    loader: HandleValue,
) -> Result<HandleValue, ProcessBuilderError<()>> {
    let factory = factory
        .resolve::<TaskFactory>(crate::authority::Rights::CREATE_PROCESS)
        .map_err(crate::process::ProcessError::from)
        .map_err(ProcessBuilderError::Process)?;
    let group = caller
        .resolve_handle::<TaskGroupObject>(
            group,
            crate::authority::Rights::TASK_GROUP_ATTACH_PROCESS,
        )
        .map_err(ProcessBuilderError::Process)?;
    let domain = caller
        .resolve_handle::<ResourceDomainObject>(
            domain,
            crate::authority::Rights::RESOURCE_DOMAIN_SPONSOR,
        )
        .map_err(ProcessBuilderError::Process)?;
    let executable = copy_executable_for_child(caller, executable)?;

    let loader = caller
        .resolve_handle::<FileObject>(loader, Rights::READ.union(Rights::EXECUTE))
        .map_err(ProcessBuilderError::Process)?;

    let reservation = caller
        .reserve_handles::<1>()
        .map_err(ProcessBuilderError::Process)?;
    let publication = match ProcessBuilder::try_publication(
        factory.object(),
        group.object(),
        domain.object(),
        executable,
        loader.object(),
    ) {
        Ok(publication) => publication,
        Err(error) => {
            caller.abort_handles(reservation);
            return Err(error);
        }
    };
    if let Err(error) = publication.object().inherit_scheduling(
        super::scheduling::ProcessScheduling::child_initial(inherited_scheduling),
        factory.rights(),
    ) {
        caller.abort_handles(reservation);
        return Err(error);
    }
    let prepared = match PreparedHandle::try_from_new_object(
        publication,
        ProcessBuilder::BASE_RIGHTS.union(
            factory.rights().intersection(
                Rights::LOCK_MEMORY
                    .union(Rights::RETAIN_PROCESS)
                    .union(crate::task::scheduling_authority::SCHEDULING_RIGHTS),
            ),
        ),
        HandleFlags::NONE,
    ) {
        Ok(prepared) => prepared,
        Err(error) => {
            caller.abort_handles(reservation);
            return Err(ProcessBuilderError::Process(error.into()));
        }
    };
    match caller.publish_handles(reservation, [prepared]) {
        Ok([value]) => Ok(value),
        Err(failure) => Err(ProcessBuilderError::Process(failure.error)),
    }
}

/// Starts a builder through the process-owned consume-and-publish transaction.
///
/// The ABI adapter supplies only the caller namespace and numeric handle. This
/// boundary binds the resolved object KOID to the reversible handle claim and
/// keeps transaction internals out of syscall dispatch.
pub(crate) fn start_process_builder(
    caller: &Process,
    value: &InvocationReceiver,
) -> Result<
    super::owner::StartedChildProcess,
    ProcessBuilderError<super::owner::ChildProcessStartError>,
> {
    let resolved = value
        .resolve::<ProcessBuilder>(crate::authority::Rights::START)
        .map_err(crate::process::ProcessError::from)
        .map_err(ProcessBuilderError::Process)?;
    let mut coordinator =
        super::owner::ProcessStartCoordinator::new(caller.clone(), value.handle(), resolved.koid());
    resolved.object().start_with(&mut coordinator)
}

/// Aborts and consumes exactly the builder authority named by `value`.
///
/// Preparation is reversible, so every builder error leaves the caller's
/// handle unchanged. A successful abort is followed only by the infallible
/// handle-consumption commit and deferred authority release.
pub(crate) fn abort_process_builder(
    caller: &Process,
    value: &InvocationReceiver,
) -> Result<(), ProcessBuilderError<()>> {
    let resolved = value
        .resolve::<ProcessBuilder>(crate::authority::Rights::REQUEST_STOP)
        .map_err(crate::process::ProcessError::from)
        .map_err(ProcessBuilderError::Process)?;
    let consumption = caller
        .prepare_handle_consumption(
            value.handle(),
            crate::authority::Rights::REQUEST_STOP,
            ObjectKind::PROCESS_BUILDER,
            resolved.koid(),
        )
        .map_err(ProcessBuilderError::Process)?;
    match resolved.object().abort() {
        Ok(()) => {
            consumption.commit_and_release();
            Ok(())
        }
        Err(error) => {
            consumption.rollback();
            Err(error)
        }
    }
}

impl private::Sealed for ProcessBuilder {}
impl private::UserExportable for ProcessBuilder {}

impl KernelObject for ProcessBuilder {
    const KIND: ObjectKind = ObjectKind::PROCESS_BUILDER;
    const SUPPORTED_RIGHTS: Rights = Self::BASE_RIGHTS
        .union(Rights::LOCK_MEMORY)
        .union(Rights::RETAIN_PROCESS)
        .union(crate::task::scheduling_authority::SCHEDULING_RIGHTS);
    const TRANSFER_CLASS: TransferClass = TransferClass::RendezvousOnly;

    fn on_zero_active_handles(&self, _retirement: &mut crate::object::ObjectRetirement) {
        self.abort_requested.store(true, Ordering::Release);
    }
}

fn require_busy_state(state: &Option<BuilderState>) {
    if !matches!(state, Some(BuilderState::Busy)) {
        builder_invariant_violation();
    }
}

fn require_commit_authorized_state(state: &Option<BuilderState>) {
    if !matches!(state, Some(BuilderState::CommitAuthorized)) {
        builder_invariant_violation();
    }
}

fn copy_name(name: &str) -> Result<String, ProcessBuilderError<()>> {
    if !valid_name(name) {
        return Err(ProcessBuilderError::InvalidString);
    }
    let mut owned = String::new();
    owned
        .try_reserve_exact(name.len())
        .map_err(|_| ProcessBuilderError::Allocation)?;
    owned.push_str(name);
    Ok(owned)
}

fn prepare_sealed_process(plan: &BuilderPlan) -> Result<PreparedProcess, ProcessBuilderError<()>> {
    if plan.thread_name.is_none() {
        return Err(ProcessBuilderError::MissingName);
    }
    let domain = plan.domain.clone();
    let loader = plan
        .loader
        .as_ref()
        .unwrap_or_else(|| builder_invariant_violation());
    let loaded = load_native(loader, domain.clone()).map_err(ProcessBuilderError::Image)?;
    let prepared = match PreparedProcess::try_new(
        loaded.image,
        plan.group.clone(),
        domain,
        loaded.address_space,
    ) {
        Ok(prepared) => prepared,
        Err(failure) => {
            let (error, address_space) = failure.into_parts();
            retire_unpublished_address_space(address_space);
            return Err(ProcessBuilderError::Process(error));
        }
    };
    prepared.process().set_initial_scheduling(plan.scheduling);
    if let Some(retention) = &plan.retention {
        prepared.process().install_retention(retention.clone());
    }
    Ok(prepared)
}

fn validate_startup_capability(
    plan: &mut BuilderPlan,
    capability: StartupCapability,
    source: &Process,
) -> Result<HandleInfo, ProcessBuilderError<()>> {
    if capability.purpose == 0
        || capability.purpose == child_root_vmar_purpose()
        || u64::from(capability.purpose)
            == hyper::abi::native::HYPER_NATIVE_STARTUP_HANDLE_PURPOSE_INITIAL_STACK_VMAR
        || u64::from(capability.purpose)
            == hyper::abi::native::HYPER_NATIVE_STARTUP_HANDLE_PURPOSE_EXECUTION_CONTEXT
        || u64::from(capability.purpose)
            == hyper::abi::native::HYPER_NATIVE_STARTUP_HANDLE_PURPOSE_OBJECT_FACTORY
        || u64::from(capability.purpose)
            == hyper::abi::native::HYPER_NATIVE_STARTUP_HANDLE_PURPOSE_SYSTEM_INFO
    {
        return Err(ProcessBuilderError::InvalidStartupPurpose);
    }
    // The coordinator mints both VMARs and the three local invocation capabilities.
    if plan.startup.len() >= MAX_USER_STARTUP_HANDLES {
        return Err(ProcessBuilderError::StartupHandleLimit);
    }
    if plan
        .startup
        .iter()
        .any(|present| present.purpose == capability.purpose)
    {
        return Err(ProcessBuilderError::DuplicateStartupPurpose);
    }
    let info = source
        .handle_info(capability.value, crate::authority::Rights::NONE)
        .map_err(ProcessBuilderError::Process)?;
    if !BuilderStorable::permits_kind_id(info.kind.get()) {
        return Err(ProcessBuilderError::UnsupportedStartupKind);
    }
    plan.startup
        .try_reserve(1)
        .map_err(|_| ProcessBuilderError::Allocation)?;
    Ok(info)
}

const fn child_root_vmar_purpose() -> u32 {
    const PURPOSE: u64 = hyper::abi::native::HYPER_NATIVE_STARTUP_HANDLE_PURPOSE_ROOT_VMAR;
    assert!(PURPOSE <= u32::MAX as u64);
    PURPOSE as u32
}

fn reserve_builder_charge(
    domain: &ResourceDomain,
) -> Result<CommittedCharge, ProcessBuilderError<()>> {
    let bytes = object_allocation_size::<ProcessBuilder>()
        .and_then(|value| {
            value.checked_add(
                MAX_USER_STARTUP_HANDLES
                    .checked_mul(core::mem::size_of::<StoredStartupCapability>())?,
            )
        })
        .and_then(|value| value.checked_add(MAX_STARTUP_DATA_BYTES))
        .and_then(|value| value.checked_add(super::builder_input::MAX_NAME_BYTES))
        .and_then(|value| u64::try_from(value).ok())
        .ok_or(ProcessBuilderError::Allocation)?;
    Ok(domain
        .reserve(
            ResourceAmount::ZERO
                .with(ResourceKind::KernelObjects, 1)
                .with(ResourceKind::KernelMemoryBytes, bytes),
        )
        .map_err(ProcessBuilderError::Resource)?
        .commit())
}

fn retire_unpublished_address_space(
    address_space: hyper::mm::UniqueFallibleArc<NativeAddressSpace>,
) {
    if let Err(failure) = NativeAddressSpace::retire_unpublished(address_space) {
        let (error, retained) = failure.into_parts();
        crate::pr_err!(
            "HypeR: retaining an aborted ProcessBuilder address space after cleanup error: {error:?}"
        );
        drop(retained);
    }
}

#[cold]
fn builder_invariant_violation() -> ! {
    crate::diagnostics::crash::fatal(format_args!(
        "HypeR: ProcessBuilder linear-state invariant violated"
    ))
}
