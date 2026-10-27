// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Atomic child Process preparation, publication, and rollback.

use crate::capability::handle_wire;

use super::handle_transactions::PreparedHandleConsumption;
use super::*;

/// Failure before a child Process becomes visible in any kernel directory.
#[derive(Debug)]
pub(crate) enum ChildProcessStartError {
    Process(ProcessError),
    Bootstrap(super::super::bootstrap::Error),
    TaskObject(super::super::objects::TaskObjectError),
    VmarObject(MemoryObjectError),
}

/// Parent-side coordinator for one consume-builder-and-start transaction.
///
/// The builder handle value and KOID identify the exact authority resolved by
/// the syscall adapter. A generation match alone is insufficient because the
/// adapter releases its table lock before the builder serializes preparation.
pub(crate) struct ProcessStartCoordinator {
    parent: Process,
    builder_handle: HandleValue,
    builder_koid: Koid,
}

impl ProcessStartCoordinator {
    pub(crate) const fn new(
        parent: Process,
        builder_handle: HandleValue,
        builder_koid: Koid,
    ) -> Self {
        Self {
            parent,
            builder_handle,
            builder_koid,
        }
    }
}

/// Published child identity returned after deferred builder authority retires.
pub(crate) struct StartedChildProcess {
    // Keep service ownership alive until the caller consumes the start result.
    _process: Process,
    _initial_thread: UserThread,
    supervisor_handle: HandleValue,
    startup_channel: HandleValue,
}

impl StartedChildProcess {
    pub(crate) const fn startup_channel(&self) -> HandleValue {
        self.startup_channel
    }
    pub(crate) const fn supervisor_handle(&self) -> HandleValue {
        self.supervisor_handle
    }
}

pub(crate) struct PreparedChildProcessStart {
    parent: Process,
    builder_handle: HandleValue,
    build: Option<SealedProcessBuild>,
    child: Process,
    child_handle_batches: alloc::vec::Vec<ProcessHandleBatchReservation>,
    prepared_startup_handles: alloc::vec::Vec<PreparedHandle>,
    retired_batch_storage: alloc::vec::Vec<RetiredHandleBatchReservationStorage>,
    retired_charge_storage: alloc::vec::Vec<alloc::vec::Vec<ChargeReservation>>,
    retired_scratch_charges: alloc::vec::Vec<CommittedCharge>,
    start_scratch_charge: Option<CommittedCharge>,
    initial_thread: Option<PreparedInitialUserThread>,
    parent_supervisor: Option<ProcessHandleReservation<2>>,
    builder_consumption: Option<PreparedHandleConsumption>,
    root_vmar_object: Option<PreparedHandle>,
    stack_vmar_object: Option<PreparedHandle>,
    invocation_handles: Option<[PreparedHandle; 3]>,
    root_vmar_address_space: FallibleArc<NativeAddressSpace>,
    root_vmar_claimed: bool,
    process_object: Option<PublishableRef<ProcessObject, KernelService>>,
    supervisor_object: Option<PreparedHandle>,
    bootstrap: Option<super::super::bootstrap::PreparedBootstrap>,
}

pub(crate) struct CommittedChildProcessStart {
    child: Process,
    initial_thread: UserThread,
    supervisor_handle: HandleValue,
    startup_channel: HandleValue,
    retired_builder: Option<InTransitCapabilities>,
}

impl Drop for CommittedChildProcessStart {
    fn drop(&mut self) {
        if self.retired_builder.is_some() {
            process_invariant_violation();
        }
    }
}

impl ProcessStartTransaction for ProcessStartCoordinator {
    type Error = ChildProcessStartError;
    type Prepared = alloc::boxed::Box<PreparedChildProcessStart>;
    type Committed = CommittedChildProcessStart;
    type Output = StartedChildProcess;

    fn prepare_start(
        &mut self,
        build: SealedProcessBuild,
    ) -> Result<Self::Prepared, StartPreparationFailure<Self::Error>> {
        PreparedChildProcessStart::prepare(
            self.parent.clone(),
            self.builder_handle,
            self.builder_koid,
            build,
        )
    }

    fn commit_start(&mut self, prepared: Self::Prepared) -> Self::Committed {
        prepared.commit()
    }

    fn finish_start(&mut self, mut committed: Self::Committed) -> Self::Output {
        match committed.retired_builder.take() {
            Some(builder) => builder.release(),
            None => process_invariant_violation(),
        }
        StartedChildProcess {
            _process: committed.child.clone(),
            _initial_thread: committed.initial_thread.clone(),
            supervisor_handle: committed.supervisor_handle,
            startup_channel: committed.startup_channel,
        }
    }

    fn cancel_start(&mut self, prepared: Self::Prepared) -> SealedProcessBuild {
        prepared.cancel()
    }
}

// The unpublished channel holds the complete launch message before any
// thread can run. Numeric IDs refer only to already reserved child slots.
#[inline(never)]
fn prepare_bootstrap(
    build: &SealedProcessBuild,
    batches: &[ProcessHandleBatchReservation],
) -> Result<(super::super::bootstrap::PreparedBootstrap, u64), ChildProcessStartError> {
    use hyper::abi::native::{HyperNativeLoaderStartup, HyperNativeStartupHandle};
    let count = build.startup_capabilities().len();
    let mut values = batches
        .iter()
        .flat_map(ProcessHandleBatchReservation::values)
        .skip(count);
    let mut next = || match values.next() {
        Some(v) => handle_wire::to_abi(*v),
        None => process_invariant_violation(),
    };
    let root_vmar = next();
    let stack_vmar = next();
    let executable = next();
    let channel = next();
    let execution_context = next();
    let object_factory = next();
    let system_info = next();
    let layout = build.child().process().image().bootstrap();
    let header = HyperNativeLoaderStartup {
        size: core::mem::size_of::<HyperNativeLoaderStartup>() as u32,
        handle_count: count as u32,
        data_size: build.data().len() as u32,
        flags: hyper::abi::native::HYPER_NATIVE_LOADER_STARTUP_REPLY as u32,
        executable,
        root_vmar,
        stack_vmar,
        stack_base: layout.stack_base,
        stack_size: layout.stack_size,
        loader_base: layout.loader_base,
        loader_size: layout.loader_size,
        execution_context,
        object_factory,
        system_info,
    };
    let records = build
        .startup_capabilities()
        .iter()
        .zip(
            batches
                .iter()
                .flat_map(ProcessHandleBatchReservation::values),
        )
        .map(|(capability, value)| HyperNativeStartupHandle {
            purpose: capability.purpose(),
            flags: 0,
            handle: handle_wire::to_abi(*value),
        });
    super::super::bootstrap::prepare(
        &build.child().process().resource_domain(),
        build.executable(),
        header,
        records,
        build.data(),
    )
    .map(|bootstrap| (bootstrap, channel))
    .map_err(ChildProcessStartError::Bootstrap)
}

impl PreparedChildProcessStart {
    // Keep fallible preparation out of the publication coordinator's frame.
    // This boundary is part of the native syscall stack budget: preparation
    // returns before abort arbitration and the infallible commit begin.
    #[inline(never)]
    #[expect(
        clippy::result_large_err,
        reason = "the error retains the complete linear build owner for allocation-free rollback"
    )]
    fn prepare(
        parent: Process,
        builder_handle: HandleValue,
        builder_koid: Koid,
        build: SealedProcessBuild,
    ) -> Result<alloc::boxed::Box<Self>, StartPreparationFailure<ChildProcessStartError>> {
        let child = build.child().process().clone();
        let invocation_handles = match child.prepare_invocation_handles() {
            Ok(handles) => handles,
            Err(error) => return Err(start_failure(ChildProcessStartError::Process(error), build)),
        };
        let startup_count = match build.startup_capabilities().len().checked_add(7) {
            Some(count) => count,
            None => {
                return Err(start_failure(
                    ChildProcessStartError::Process(ProcessError::Allocation),
                    build,
                ));
            }
        };
        let batch_maximum = HandleBatchReservationStorage::maximum_count();
        let batch_count = match startup_count
            .checked_add(batch_maximum.saturating_sub(1))
            .map(|rounded| rounded / batch_maximum)
        {
            Some(count) => count,
            None => {
                return Err(start_failure(
                    ChildProcessStartError::Process(ProcessError::Allocation),
                    build,
                ));
            }
        };
        let start_scratch_charge = match reserve_start_scratch(&child, startup_count, batch_count) {
            Ok(charge) => charge,
            Err(error) => {
                return Err(start_failure(ChildProcessStartError::Process(error), build));
            }
        };
        // The transaction owns over 1.5 KiB of rollback state. Keep it in
        // charged heap storage so Result/phase handoffs cannot retain multiple
        // copies below handle-table allocation and IRQ-tail scheduling.
        let storage = match hyper::mm::try_box_uninit::<Self>() {
            Ok(storage) => storage,
            Err(_) => {
                return Err(start_failure(
                    ChildProcessStartError::Process(ProcessError::Allocation),
                    build,
                ));
            }
        };
        let mut child_handle_batches = match build
            .child()
            .reserve_startup_handle_batches(startup_count)
        {
            Ok(batches) => batches,
            Err(error) => return Err(start_failure(ChildProcessStartError::Process(error), build)),
        };

        if child_handle_batches.len() != batch_count {
            process_invariant_violation();
        }
        let mut prepared_startup_handles = alloc::vec::Vec::new();
        let mut retired_batch_storage = alloc::vec::Vec::new();
        let mut retired_charge_storage = alloc::vec::Vec::new();
        let mut retired_scratch_charges = alloc::vec::Vec::new();
        let scratch_ready = prepared_startup_handles
            .try_reserve_exact(startup_count)
            .and_then(|()| retired_batch_storage.try_reserve_exact(batch_count))
            .and_then(|()| retired_charge_storage.try_reserve_exact(batch_count))
            .and_then(|()| retired_scratch_charges.try_reserve_exact(batch_count));
        if scratch_ready.is_err() {
            abort_child_handle_batches(&child, &mut child_handle_batches);
            return Err(start_failure(
                ChildProcessStartError::Process(ProcessError::Allocation),
                build,
            ));
        }
        let (bootstrap, channel) = match prepare_bootstrap(&build, &child_handle_batches) {
            Ok(prepared) => prepared,
            Err(error) => {
                abort_child_handle_batches(&child, &mut child_handle_batches);
                return Err(start_failure(error, build));
            }
        };

        let initial_thread = match build.child().prepare_initial_user_thread(
            build.thread_name(),
            build.affinity(),
            channel,
        ) {
            Ok(thread) => thread,
            Err(error) => {
                abort_child_handle_batches(&child, &mut child_handle_batches);
                return Err(start_failure(ChildProcessStartError::Process(error), build));
            }
        };
        let parent_supervisor = match parent.reserve_handles::<2>() {
            Ok(reservation) => reservation,
            Err(error) => {
                drop(initial_thread);
                abort_child_handle_batches(&child, &mut child_handle_batches);
                return Err(start_failure(ChildProcessStartError::Process(error), build));
            }
        };
        let builder_consumption = match parent.prepare_handle_consumption(
            builder_handle,
            Rights::START,
            crate::object::ObjectKind::PROCESS_BUILDER,
            builder_koid,
        ) {
            Ok(consumption) => consumption,
            Err(error) => {
                parent.abort_handles(parent_supervisor);
                drop(initial_thread);
                abort_child_handle_batches(&child, &mut child_handle_batches);
                return Err(start_failure(ChildProcessStartError::Process(error), build));
            }
        };
        let root_vmar_address_space = build.child().address_space_owner();
        let stack_vmar_object = match prepare_stack_vmar(&child, root_vmar_address_space.clone()) {
            Ok(handle) => handle,
            Err(error) => {
                builder_consumption.rollback();
                parent.abort_handles(parent_supervisor);
                drop(initial_thread);
                abort_child_handle_batches(&child, &mut child_handle_batches);
                return Err(start_failure(error, build));
            }
        };
        let root_vmar_publication = match VmarObject::try_root_publication(
            root_vmar_address_space.clone(),
            &child.resource_domain(),
        ) {
            Ok(publication) => publication,
            Err(error) => {
                builder_consumption.rollback();
                parent.abort_handles(parent_supervisor);
                drop(initial_thread);
                abort_child_handle_batches(&child, &mut child_handle_batches);
                return Err(start_failure(
                    ChildProcessStartError::VmarObject(error),
                    build,
                ));
            }
        };
        let root_vmar_object = match PreparedHandle::try_from_new_object(
            root_vmar_publication,
            if build.memory_lock_allowed() {
                VmarObject::ROOT_RIGHTS.union(Rights::LOCK_MEMORY)
            } else {
                VmarObject::ROOT_RIGHTS
            },
            HandleFlags::NONE,
        ) {
            Ok(handle) => handle,
            Err(error) => {
                VmarObject::abort_root_publication(&root_vmar_address_space);
                builder_consumption.rollback();
                parent.abort_handles(parent_supervisor);
                drop(initial_thread);
                abort_child_handle_batches(&child, &mut child_handle_batches);
                return Err(start_failure(
                    ChildProcessStartError::Process(error.into()),
                    build,
                ));
            }
        };
        let process_object = match ProcessObject::try_service(&child) {
            Ok(object) => object,
            Err(error) => {
                drop(root_vmar_object);
                VmarObject::abort_root_publication(&root_vmar_address_space);
                builder_consumption.rollback();
                parent.abort_handles(parent_supervisor);
                drop(initial_thread);
                abort_child_handle_batches(&child, &mut child_handle_batches);
                return Err(start_failure(
                    ChildProcessStartError::TaskObject(error),
                    build,
                ));
            }
        };
        let publication = process_object.publication();
        let supervisor_object = match PreparedHandle::try_from_new_object(
            publication,
            ProcessObject::SUPERVISOR_RIGHTS,
            HandleFlags::NONE,
        ) {
            Ok(handle) => handle,
            Err(error) => {
                drop(root_vmar_object);
                VmarObject::abort_root_publication(&root_vmar_address_space);
                builder_consumption.rollback();
                parent.abort_handles(parent_supervisor);
                drop(initial_thread);
                abort_child_handle_batches(&child, &mut child_handle_batches);
                return Err(start_failure(
                    ChildProcessStartError::Process(error.into()),
                    build,
                ));
            }
        };

        Ok(alloc::boxed::Box::write(
            storage,
            Self {
                parent,
                builder_handle,
                build: Some(build),
                child,
                child_handle_batches,
                prepared_startup_handles,
                retired_batch_storage,
                retired_charge_storage,
                retired_scratch_charges,
                start_scratch_charge: Some(start_scratch_charge),
                initial_thread: Some(initial_thread),
                parent_supervisor: Some(parent_supervisor),
                builder_consumption: Some(builder_consumption),
                root_vmar_object: Some(root_vmar_object),
                stack_vmar_object: Some(stack_vmar_object),
                invocation_handles: Some(invocation_handles),
                root_vmar_address_space,
                root_vmar_claimed: true,
                process_object: Some(process_object),
                supervisor_object: Some(supervisor_object),
                bootstrap: Some(bootstrap),
            },
        ))
    }

    // Publication and rollback run after preparation. Keep their scratch
    // frames out of the coordinator while it calls deeper preparation paths;
    // IRQ preemption and the incoming scheduler tail also need stack headroom.
    #[inline(never)]
    fn commit(mut self: alloc::boxed::Box<Self>) -> CommittedChildProcessStart {
        let build = match self.build.take() {
            Some(build) => build,
            None => process_invariant_violation(),
        };
        let process_name = ProcessNameSnapshot::from_validated(build.thread_name());
        let (prepared_child, startup_capabilities) = build.into_parts();
        for capability in startup_capabilities {
            let (mut handles, storage_charge) = capability.take_authority().into_prepared_handles();
            if handles.len() != 1 {
                process_invariant_violation();
            }
            let handle = match handles.pop() {
                Some(handle) => handle,
                None => process_invariant_violation(),
            };
            self.prepared_startup_handles.push(handle);
            drop(handles);
            drop(storage_charge);
        }
        let root_vmar = match self.root_vmar_object.take() {
            Some(root_vmar) => root_vmar,
            None => process_invariant_violation(),
        };
        self.prepared_startup_handles.push(root_vmar);
        let stack_vmar = match self.stack_vmar_object.take() {
            Some(handle) => handle,
            None => process_invariant_violation(),
        };
        self.prepared_startup_handles.push(stack_vmar);
        let bootstrap = match self.bootstrap.take() {
            Some(b) => b,
            None => process_invariant_violation(),
        };
        self.prepared_startup_handles.push(bootstrap.executable);
        self.prepared_startup_handles.push(bootstrap.child_channel);
        let invocation_handles = match self.invocation_handles.take() {
            Some(handles) => handles,
            None => process_invariant_violation(),
        };
        self.prepared_startup_handles.extend(invocation_handles);
        self.prepared_startup_handles.reverse();
        publish_unpublished_startup_handles(&mut self);

        let initial_thread = match self.initial_thread.take() {
            Some(thread) => thread,
            None => process_invariant_violation(),
        };
        let (initial_thread, thread_id, terminal) = initial_thread.publish();
        if terminal.is_some() {
            process_invariant_violation();
        }
        let process_object = match self.process_object.take() {
            Some(object) => object,
            None => process_invariant_violation(),
        };
        let child = prepared_child.publish(process_object, process_name);
        let (supervisor_handle, startup_channel, retired_builder) =
            commit_parent_builder_replacement(&mut self, bootstrap.parent_channel);
        child.commit_initial_execution(thread_id);
        let scratch_charge = self.start_scratch_charge.take();
        self.root_vmar_claimed = false;
        let committed = CommittedChildProcessStart {
            child,
            initial_thread,
            supervisor_handle,
            startup_channel,
            retired_builder: Some(retired_builder),
        };
        drop(self);
        drop(scratch_charge);
        committed
    }

    #[inline(never)]
    fn cancel(mut self: alloc::boxed::Box<Self>) -> SealedProcessBuild {
        let supervisor_object = self.supervisor_object.take();
        drop(supervisor_object);
        drop(self.process_object.take());
        drop(self.bootstrap.take());
        if let Some(consumption) = self.builder_consumption.take() {
            consumption.rollback();
        }
        if let Some(reservation) = self.parent_supervisor.take() {
            self.parent.abort_handles(reservation);
        }
        drop(self.initial_thread.take());
        drop(self.root_vmar_object.take());
        drop(self.stack_vmar_object.take());
        drop(self.invocation_handles.take());
        if self.root_vmar_claimed {
            VmarObject::abort_root_publication(&self.root_vmar_address_space);
            self.root_vmar_claimed = false;
        }
        abort_child_handle_batches(&self.child, &mut self.child_handle_batches);
        self.prepared_startup_handles.clear();
        let scratch_charge = self.start_scratch_charge.take();
        let build = match self.build.take() {
            Some(build) => build,
            None => process_invariant_violation(),
        };
        drop(self);
        drop(scratch_charge);
        build
    }
}

impl Drop for PreparedChildProcessStart {
    fn drop(&mut self) {
        if self.build.is_some()
            || !self.child_handle_batches.is_empty()
            || !self.prepared_startup_handles.is_empty()
            || self.initial_thread.is_some()
            || self.parent_supervisor.is_some()
            || self.builder_consumption.is_some()
            || self.root_vmar_object.is_some()
            || self.stack_vmar_object.is_some()
            || self.invocation_handles.is_some()
            || self.root_vmar_claimed
            || self.process_object.is_some()
            || self.supervisor_object.is_some()
            || self.bootstrap.is_some()
            || self.start_scratch_charge.is_some()
        {
            process_invariant_violation();
        }
    }
}

// Resource-domain reservation has its own temporary accounting path. Keep it
// off the coordinator frame used later for thread and handle preparation.
#[inline(never)]
fn reserve_start_scratch(
    child: &Process,
    startup_count: usize,
    batch_count: usize,
) -> Result<CommittedCharge, ProcessError> {
    let allocations = [
        (1, core::mem::size_of::<PreparedChildProcessStart>()),
        (
            batch_count,
            core::mem::size_of::<ProcessHandleBatchReservation>(),
        ),
        (startup_count, core::mem::size_of::<PreparedHandle>()),
        (
            batch_count,
            core::mem::size_of::<RetiredHandleBatchReservationStorage>(),
        ),
        (
            batch_count,
            core::mem::size_of::<alloc::vec::Vec<ChargeReservation>>(),
        ),
        (batch_count, core::mem::size_of::<CommittedCharge>()),
    ];
    let bytes = allocations
        .into_iter()
        .try_fold(0usize, |total, (count, size)| {
            count
                .checked_mul(size)
                .and_then(|allocation| total.checked_add(allocation))
        });
    let bytes = bytes
        .and_then(|bytes| u64::try_from(bytes).ok())
        .ok_or(ProcessError::Allocation)?;
    Ok(child
        .resource_domain()
        .reserve(ResourceAmount::ZERO.with(ResourceKind::KernelMemoryBytes, bytes))?
        .commit())
}

#[cold]
#[inline(never)]
fn start_failure(
    error: ChildProcessStartError,
    build: SealedProcessBuild,
) -> StartPreparationFailure<ChildProcessStartError> {
    StartPreparationFailure { error, build }
}

// Batch rollback is a separate phase; its release/accounting scratch must
// not enlarge the successful preparation frame.
#[inline(never)]
fn abort_child_handle_batches(
    child: &Process,
    batches: &mut alloc::vec::Vec<ProcessHandleBatchReservation>,
) {
    for batch in batches.drain(..) {
        child.abort_handle_batch(batch);
    }
}

fn publish_unpublished_startup_handles(prepared: &mut PreparedChildProcessStart) {
    prepared.child_handle_batches.reverse();
    prepared.child.inner.state.with(|state| {
        if state.lifecycle.phase() != ProcessPhase::Prepared {
            process_invariant_violation();
        }
        while let Some(mut reservation) = prepared.child_handle_batches.pop() {
            reservation.require_owner(&prepared.child);
            let token = match reservation.reservation.take() {
                Some(token) => token,
                None => process_invariant_violation(),
            };
            let retired = prepared.child.inner.handles.with(|table| {
                token.publish_from_reversed(table, &mut prepared.prepared_startup_handles)
            });
            prepared.retired_batch_storage.push(retired);

            let mut charges = match reservation.handle_charges.take() {
                Some(charges) => charges,
                None => process_invariant_violation(),
            };
            let record = match reservation.record.take() {
                Some(record) => record,
                None => process_invariant_violation(),
            };
            state.handle_accounting.install(record, &mut charges);
            prepared.retired_charge_storage.push(charges);
            match reservation.scratch_charge.take() {
                Some(charge) => prepared.retired_scratch_charges.push(charge),
                None => process_invariant_violation(),
            }
        }
    });
    if !prepared.prepared_startup_handles.is_empty() {
        process_invariant_violation();
    }
    prepared.retired_batch_storage.clear();
    prepared.retired_charge_storage.clear();
    prepared.retired_scratch_charges.clear();
}

fn commit_parent_builder_replacement(
    prepared: &mut PreparedChildProcessStart,
    startup_channel: PreparedHandle,
) -> (HandleValue, HandleValue, InTransitCapabilities) {
    let mut source = match prepared.builder_consumption.take() {
        Some(mut consumption) => match consumption.transfer.take() {
            Some(source) => source,
            None => process_invariant_violation(),
        },
        None => process_invariant_violation(),
    };
    if source.process.id() != prepared.parent.id() {
        process_invariant_violation();
    }
    let mut destination = match prepared.parent_supervisor.take() {
        Some(destination) => destination,
        None => process_invariant_violation(),
    };
    destination.require_owner(&prepared.parent);
    let mut supervisor = match prepared.supervisor_object.take() {
        Some(supervisor) => Some(supervisor),
        None => process_invariant_violation(),
    };
    let mut retired_transfer_storage = None;
    let mut builder_batch = None;
    let mut supervisor_value = None;
    let mut startup_channel = Some(startup_channel);
    prepared.parent.inner.state.with(|state| {
        if source.moved_values.as_slice() != [prepared.builder_handle] {
            process_invariant_violation();
        }
        if !state.handle_accounting.is_live(prepared.builder_handle) {
            process_invariant_violation();
        }
        let claim = match source.claim.take() {
            Some(claim) => claim,
            None => process_invariant_violation(),
        };
        let destination_token = match destination.reservation.take() {
            Some(reservation) => reservation,
            None => process_invariant_violation(),
        };
        let published_supervisor = match supervisor.take() {
            Some(supervisor) => supervisor,
            None => process_invariant_violation(),
        };
        let ((detached_builder, retired), values) = prepared.parent.inner.handles.with(|table| {
            let detached = claim.commit_with_storage(table);
            let values = destination_token.publish(
                table,
                [
                    published_supervisor,
                    match startup_channel.take() {
                        Some(h) => h,
                        None => process_invariant_violation(),
                    },
                ],
            );
            (detached, values)
        });
        builder_batch = Some(detached_builder);
        retired_transfer_storage = Some(retired);
        supervisor_value = Some(values);

        source.detach_source_charges(&mut state.handle_accounting);

        let mut charges = match destination.handle_charges.take() {
            Some(charges) => charges,
            None => process_invariant_violation(),
        };
        let record = match destination.record.take() {
            Some(record) => record,
            None => process_invariant_violation(),
        };
        if charges.len() != 2 {
            process_invariant_violation();
        }
        state.handle_accounting.install(record, &mut charges);
        destination.handle_charges = Some(charges);
    });

    drop(retired_transfer_storage.take());
    drop(source.entry_charge.take());
    source.release_scratch();
    let storage_charge = match source.handle_charge.take() {
        Some(charge) => charge,
        None => process_invariant_violation(),
    };
    drop(destination.handle_charges.take());
    drop(destination.record.take());
    let detached_builder = match builder_batch.take() {
        Some(builder) => builder,
        None => process_invariant_violation(),
    };
    let value = match supervisor_value {
        Some(value) => value,
        None => process_invariant_violation(),
    };
    (
        value[0],
        value[1],
        InTransitCapabilities::new(detached_builder, storage_charge),
    )
}

/// The loader owns the VMAR reservation throughout a retryable start. The
/// prepared handle only wraps it; cancellation drops the wrapper, not the VMAR.
fn prepare_stack_vmar(
    child: &Process,
    address_space: FallibleArc<NativeAddressSpace>,
) -> Result<PreparedHandle, ChildProcessStartError> {
    let token = child
        .image()
        .initial_stack_vmar()
        .ok_or(ChildProcessStartError::VmarObject(
            MemoryObjectError::AllocationSize,
        ))?;
    let object = VmarObject::try_existing(address_space, token, &child.resource_domain())
        .map_err(ChildProcessStartError::VmarObject)?;
    let publication = ObjectPublication::try_new(object)
        .map_err(|error| ChildProcessStartError::VmarObject(MemoryObjectError::Object(error)))?;
    PreparedHandle::try_from_new_object(publication, VmarObject::ROOT_RIGHTS, HandleFlags::NONE)
        .map_err(|error| ChildProcessStartError::Process(error.into()))
}
