// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Process-facing `ByteChannel` transactions through real native mappings.

use crate::capability::handle_wire;

use alloc::boxed::Box;
use core::sync::atomic::{AtomicUsize, Ordering};

use hyper::mm::PAGE_SIZE;

use {
    crate::accounting::ResourceDomain, crate::accounting::ResourceKind,
    crate::accounting::ResourceLimits,
};
use {
    crate::capability::HandleError, crate::capability::HandleFlags, crate::capability::HandleValue,
    crate::capability::PreparedHandle, crate::capability::Rights,
};
use {
    crate::ipc::ByteChannelError, crate::ipc::ByteChannelReadOutcome,
    crate::ipc::ByteChannelServiceError, crate::ipc::CapabilityChannel,
    crate::ipc::CapabilityChannelServiceError, crate::ipc::CapabilityDeliveryInfo,
    crate::ipc::CapabilityReceiveOutcome, crate::ipc::byte_channel_create,
    crate::ipc::byte_channel_read, crate::ipc::byte_channel_write,
    crate::ipc::capability_channel_receive, crate::ipc::capability_channel_try_send,
};
use {
    crate::mm::user_space::UserAddress, crate::mm::user_space::UserSlice,
    crate::mm::user_space::fail_exposed_write_after_copy_for_test,
    crate::mm::user_space::prepare_native_entry_self_test,
};
use {crate::object::Event, crate::object::KernelObject, crate::object::ObjectPublication};
use {
    crate::process::MachineAbi, crate::process::PreparedProcess, crate::process::Process,
    crate::process::ProcessError, crate::process::ProcessImage, crate::process::ProcessObject,
    crate::process::ProcessPhase, crate::process::TaskGroup, crate::process::TerminalReason,
};

const IMAGE_BASE: u64 = 0x80_0000;
const SCRATCH_BASE: u64 = IMAGE_BASE + PAGE_SIZE * 2;
const TEST_CODE: [u8; 4] = [0x00, 0x00, 0x20, 0xd4];

pub(super) enum Error {
    AddressSpace,
    CapabilityService(CapabilityChannelServiceError),
    Construction,
    Group,
    Process(ProcessError),
    Quiescence(super::support::QuiescenceError),
    Scheduler(crate::task::scheduler::Error),
    Service(ByteChannelServiceError),
    Sleep(crate::task::SleepError),
    State(usize),
}

impl core::fmt::Debug for Error {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::AddressSpace => formatter.write_str("AddressSpace"),
            Self::CapabilityService(error) => formatter
                .debug_tuple("CapabilityService")
                .field(error)
                .finish(),
            Self::Construction => formatter.write_str("Construction"),
            Self::Group => formatter.write_str("Group"),
            Self::Process(error) => formatter.debug_tuple("Process").field(error).finish(),
            Self::Quiescence(error) => formatter.debug_tuple("Quiescence").field(error).finish(),
            Self::Scheduler(error) => formatter.debug_tuple("Scheduler").field(error).finish(),
            Self::Service(error) => formatter.debug_tuple("Service").field(error).finish(),
            Self::Sleep(error) => formatter.debug_tuple("Sleep").field(error).finish(),
            Self::State(stage) => formatter.debug_tuple("State").field(stage).finish(),
        }
    }
}

impl From<ProcessError> for Error {
    fn from(error: ProcessError) -> Self {
        Self::Process(error)
    }
}

impl From<ByteChannelServiceError> for Error {
    fn from(error: ByteChannelServiceError) -> Self {
        Self::Service(error)
    }
}

impl From<CapabilityChannelServiceError> for Error {
    fn from(error: CapabilityChannelServiceError) -> Self {
        Self::CapabilityService(error)
    }
}

impl From<super::support::QuiescenceError> for Error {
    fn from(error: super::support::QuiescenceError) -> Self {
        Self::Quiescence(error)
    }
}

impl From<crate::task::scheduler::Error> for Error {
    fn from(error: crate::task::scheduler::Error) -> Self {
        Self::Scheduler(error)
    }
}

impl From<crate::task::SleepError> for Error {
    fn from(error: crate::task::SleepError) -> Self {
        Self::Sleep(error)
    }
}

pub(super) fn run() -> Result<(), Error> {
    let domain =
        ResourceDomain::try_new_root(ResourceLimits::UNLIMITED).map_err(|_| Error::Construction)?;
    let group = TaskGroup::try_new(&domain).map_err(|_| Error::Group)?;
    let process = create_process(&domain, &group)?;

    verify_memory_authority_attenuation(&process, &domain)?;
    verify_guest_memory_authority(&process, &domain)?;
    super::device_authority::verify(&process).map_err(|_| Error::State(60))?;
    verify_handle_accounting_churn(&process)?;
    verify_partial_charge_records(&process)?;
    verify_handle_quota_rollback(&process, ResourceLimits::UNLIMITED)?;
    verify_handle_admission_after_stop(&domain, &group)?;
    verify_atomic_handle_replacement(&process)?;
    verify_fifo_and_buffer_contract(&process)?;
    verify_copy_failure_restores_message(&process)?;
    verify_cross_process_capability_transfer(&process, &domain, &group)?;

    let retained_space = process.address_space_owner()?;
    let report = process.request_stop(TerminalReason::Requested);
    if !report.newly_requested || !report.dispatch_complete {
        return Err(Error::State(20));
    }
    crate::task::sleep_ms(20)?;
    if process.snapshot().phase != ProcessPhase::Retiring {
        return Err(Error::State(21));
    }
    drop(retained_space);
    retire_process(&process)?;
    if domain.usage().total(ResourceKind::Handles) != 0 {
        return Err(Error::State(22));
    }
    group.request_stop().map_err(|_| Error::Group)?;
    group.finish_retirement().map_err(|_| Error::Group)
}

/// Exercise actual handle resolution and VMAR creation: ordinary mapping
/// authority must never mint residency authority on a derived VMAR.
fn verify_memory_authority_attenuation(
    process: &Process,
    domain: &ResourceDomain,
) -> Result<(), Error> {
    use crate::capability::HandleError;
    use crate::process::TaskFactory;
    use {
        crate::mm::user_space::VmarObject, crate::mm::user_space::allocate_vmar,
        crate::mm::user_space::set_memory_residency,
    };

    let factory = process.create_object(
        TaskFactory::try_new(domain).map_err(|_| Error::Construction)?,
        TaskFactory::SUPPORTED_RIGHTS,
    )?;
    let reduced_factory =
        process.duplicate_handle(factory, Rights::CREATE_PROCESS.union(Rights::DUPLICATE))?;
    if !matches!(
        process.resolve_handle::<TaskFactory>(reduced_factory, Rights::LOCK_MEMORY),
        Err(ProcessError::Handle(HandleError::AccessDenied))
    ) || !matches!(
        process.duplicate_handle(
            reduced_factory,
            Rights::CREATE_PROCESS.union(Rights::LOCK_MEMORY)
        ),
        Err(ProcessError::Handle(HandleError::AccessDenied))
    ) {
        return Err(Error::State(80));
    }
    process.close_handle(reduced_factory)?;
    process.close_handle(factory)?;

    let publication = VmarObject::try_root_publication(process.address_space_owner()?, domain)
        .map_err(|_| Error::Construction)?;
    let prepared = PreparedHandle::try_from_new_object(
        publication,
        VmarObject::ROOT_RIGHTS.union(Rights::LOCK_MEMORY),
        HandleFlags::NONE,
    )
    .map_err(|_| Error::Construction)?;
    let reservation = process.reserve_handles::<1>()?;
    let [root] = process
        .publish_handles(reservation, [prepared])
        .map_err(|failure| failure.error)?;
    let ordinary = process.duplicate_handle(root, VmarObject::ROOT_RIGHTS)?;
    let (child, _) = allocate_vmar(
        process,
        &process.resolve_invocation(ordinary)?,
        IMAGE_BASE + PAGE_SIZE * 3,
        PAGE_SIZE,
        true,
    )
    .map_err(|_| Error::State(85))?;
    if !matches!(
        process.resolve_handle::<VmarObject>(child, Rights::LOCK_MEMORY),
        Err(ProcessError::Handle(HandleError::AccessDenied))
    ) || set_memory_residency(
        process,
        &process.resolve_invocation(child)?,
        0,
        0,
        true,
        true,
    )
    .is_ok()
    {
        return Err(Error::State(81));
    }
    let (allowed_child, _) = allocate_vmar(
        process,
        &process.resolve_invocation(root)?,
        IMAGE_BASE + PAGE_SIZE * 4,
        PAGE_SIZE,
        true,
    )
    .map_err(|_| Error::State(86))?;
    process.resolve_handle::<VmarObject>(allowed_child, Rights::LOCK_MEMORY)?;
    // Restrict a child to only residency operations; MAP is independent.
    let lock_only = process.duplicate_handle(allowed_child, Rights::LOCK_MEMORY)?;
    if !matches!(
        process.resolve_handle::<VmarObject>(lock_only, Rights::MAP),
        Err(ProcessError::Handle(HandleError::AccessDenied))
    ) {
        return Err(Error::State(82));
    }
    set_memory_residency(
        process,
        &process.resolve_invocation(lock_only)?,
        0,
        0,
        true,
        true,
    )
    .map_err(|_| Error::State(83))?;
    set_memory_residency(
        process,
        &process.resolve_invocation(lock_only)?,
        0,
        0,
        true,
        false,
    )
    .map_err(|_| Error::State(84))?;
    for handle in [lock_only, allowed_child, child, ordinary, root] {
        process.close_handle(handle)?;
    }
    Ok(())
}

/// Device payload authority is independent from mapping authority and cannot
/// be restored through a stale or reduced handle.
fn verify_guest_memory_authority(process: &Process, domain: &ResourceDomain) -> Result<(), Error> {
    use crate::mm::user_space::VmoObject;
    use crate::vm::service::Error as VmError;
    use {crate::vm::objects::GuestMemoryObject, crate::vm::objects::guest_memory_access};

    let vmo = VmoObject::try_new_writable(PAGE_SIZE, domain).map_err(|_| Error::Construction)?;
    let grant = GuestMemoryObject::try_new(&vmo, domain).map_err(|_| Error::Construction)?;
    let full = process.create_object(grant, GuestMemoryObject::SUPPORTED_RIGHTS)?;
    let read_only = process.duplicate_handle(full, Rights::READ)?;
    let write_only = process.duplicate_handle(full, Rights::WRITE)?;
    let map_only = process.duplicate_handle(full, Rights::MAP)?;
    let payload = write_user(process, 0, b"ring")?;
    if guest_memory_access::scalar(&process.resolve_invocation(map_only)?, false, 0, 2, 0)
        != Err(VmError::AccessDenied)
        || guest_memory_access::scalar(&process.resolve_invocation(read_only)?, true, 0, 2, 1)
            != Err(VmError::AccessDenied)
        || guest_memory_access::scalar(&process.resolve_invocation(write_only)?, false, 0, 2, 0)
            != Err(VmError::AccessDenied)
        || guest_memory_access::transfer(
            process,
            &process.resolve_invocation(read_only)?,
            true,
            0,
            payload,
        ) != Err(VmError::AccessDenied)
        || guest_memory_access::transfer(
            process,
            &process.resolve_invocation(write_only)?,
            false,
            0,
            payload,
        ) != Err(VmError::AccessDenied)
    {
        return Err(Error::State(90));
    }
    if guest_memory_access::transfer(
        process,
        &process.resolve_invocation(write_only)?,
        true,
        4094,
        payload,
    ) != Err(VmError::InvalidArgument)
    {
        return Err(Error::State(91));
    }
    guest_memory_access::transfer(
        process,
        &process.resolve_invocation(write_only)?,
        true,
        0,
        payload,
    )
    .map_err(|_| Error::State(92))?;
    if guest_memory_access::scalar(&process.resolve_invocation(read_only)?, false, 0, 4, 0)
        != Ok(u64::from(u32::from_le_bytes(*b"ring")))
    {
        return Err(Error::State(93));
    }
    for handle in [read_only, write_only, map_only, full] {
        process.close_handle(handle)?;
    }
    if !matches!(
        process.resolve_invocation(full),
        Err(ProcessError::Handle(HandleError::InvalidHandle))
    ) {
        return Err(Error::State(94));
    }
    Ok(())
}

const CAPABILITY_RECORD_BYTES: usize =
    core::mem::size_of::<hyper::abi::native::HyperNativeCapabilityDisposition>();
const CAPABILITY_MESSAGE: &[u8] = b"delegated";

struct CapabilityReceiveContext {
    process: Process,
    endpoint: HandleValue,
    byte_output: UserSlice,
    slot_output: UserSlice,
    status: AtomicUsize,
}

fn verify_cross_process_capability_transfer(
    source: &Process,
    domain: &ResourceDomain,
    group: &TaskGroup,
) -> Result<(), Error> {
    let destination = create_process(domain, group)?;
    let fixed_reservation = destination.reserve_handles::<1>()?;
    if fixed_reservation.belongs_to(source) || !fixed_reservation.belongs_to(&destination) {
        destination.abort_handles(fixed_reservation);
        return Err(Error::State(35));
    }
    destination.abort_handles(fixed_reservation);
    let batch_reservation = destination.reserve_handle_batch(1)?;
    if batch_reservation.belongs_to(source) || !batch_reservation.belongs_to(&destination) {
        destination.abort_handle_batch(batch_reservation);
        return Err(Error::State(36));
    }
    destination.abort_handle_batch(batch_reservation);
    let (sender_object, receiver_object) =
        CapabilityChannel::try_pair(domain).map_err(CapabilityChannelServiceError::Channel)?;
    let channel_rights = <CapabilityChannel as KernelObject>::SUPPORTED_RIGHTS;
    let sender = source.create_object(sender_object, channel_rights)?;
    let receiver = destination.create_object(receiver_object, channel_rights)?;
    let transferred_rights = Rights::WAIT.union(Rights::INSPECT);
    let event = Event::try_new(domain).map_err(|_| Error::Construction)?;
    let event = source.create_object(event, transferred_rights.union(Rights::TRANSFER))?;

    let source_bytes = write_user(source, 0x300, CAPABILITY_MESSAGE)?;
    let source_record = scratch(0x340, CAPABILITY_RECORD_BYTES as u64)?;
    source.copy_to_user(source_record, &send_disposition(event, transferred_rights))?;
    let destination_bytes = scratch(0x380, 32)?;
    let destination_record = scratch(0x3c0, CAPABILITY_RECORD_BYTES as u64)?;
    destination.copy_to_user(destination_record, &receive_slot(transferred_rights))?;

    // Fail after both user outputs are pinned, before publishing the receiver.
    // The same endpoint and buffers must remain usable for the transfer below;
    // final Process retirement must not retain any output mapping pins.
    let receiver_invocation = destination.resolve_invocation(receiver)?;
    let handle_count = domain.usage().total(ResourceKind::Handles);
    domain
        .set_local_limits(ResourceLimits::UNLIMITED.with(ResourceKind::Handles, handle_count))
        .map_err(|_| Error::Construction)?;
    let rejected = capability_channel_receive(
        &destination,
        &receiver_invocation,
        0,
        Some(destination_bytes),
        Some(destination_record),
        || false,
    );
    domain
        .set_local_limits(ResourceLimits::UNLIMITED)
        .map_err(|_| Error::Construction)?;
    if !matches!(
        rejected,
        Err(CapabilityChannelServiceError::Process(
            ProcessError::Resource(_)
        ))
    ) || domain.usage().total(ResourceKind::Handles) != handle_count
    {
        return Err(Error::State(95));
    }
    drop(receiver_invocation);

    let context = Box::new(CapabilityReceiveContext {
        process: destination.clone(),
        endpoint: receiver,
        byte_output: destination_bytes,
        slot_output: destination_record,
        status: AtomicUsize::new(0),
    });
    let context = Box::into_raw(context);
    let worker = match crate::task::scheduler::kthread_create(
        "capability-receiver",
        capability_receive_worker,
        context.expose_provenance(),
    ) {
        Ok(worker) => worker,
        Err(error) => {
            // SAFETY: no Thread received the pointer when construction failed.
            drop(unsafe { Box::from_raw(context) });
            return Err(error.into());
        }
    };
    crate::task::scheduler::thread_ready(worker)?;

    let sender_object = source.resolve_handle::<CapabilityChannel>(sender, Rights::WRITE)?;
    if !crate::task::wait_for_test_progress(crate::task::TEST_PROGRESS_TIMEOUT_NS, || {
        Ok::<_, Error>(
            sender_object.object().signal_level_for_test()
                & CapabilityChannel::PEER_RECEIVING.bits()
                != 0,
        )
    })? {
        return Err(Error::State(23));
    }
    capability_channel_try_send(
        source,
        &source.resolve_invocation(sender)?,
        Some(source_bytes),
        Some(source_record),
    )?;

    // SAFETY: the allocation remains owned by this test until worker
    // quiescence, and all shared observations use atomics or immutable fields.
    let receive_status = unsafe { &(*context).status };
    if !crate::task::wait_for_test_progress(crate::task::TEST_PROGRESS_TIMEOUT_NS, || {
        Ok::<_, Error>(receive_status.load(Ordering::Acquire) != 0)
    })? {
        return Err(Error::State(24));
    }
    super::support::quiesce_workers()?;
    let status = receive_status.load(Ordering::Acquire);
    // SAFETY: scheduler quiescence proves the worker returned and its exit
    // trampoline can no longer access the boxed context.
    drop(unsafe { Box::from_raw(context) });
    if status != 1 {
        return Err(Error::State(25));
    }
    if source.handle_info(event, Rights::NONE).is_ok() {
        return Err(Error::State(26));
    }

    let mut received_bytes = [0; 32];
    destination.copy_from_user(destination_bytes, &mut received_bytes)?;
    if received_bytes.get(..CAPABILITY_MESSAGE.len()) != Some(CAPABILITY_MESSAGE) {
        return Err(Error::State(27));
    }
    let mut received_record = [0; CAPABILITY_RECORD_BYTES];
    destination.copy_from_user(destination_record, &mut received_record)?;
    let received =
        handle_wire::from_abi(read_u64(&received_record, 0)?).map_err(|_| Error::State(28))?;
    let info = destination.handle_info(received, transferred_rights)?;
    if info.kind.get() != hyper::abi::native::HYPER_NATIVE_OBJECT_EVENT
        || info.rights != transferred_rights
    {
        return Err(Error::State(29));
    }

    // The inspector belongs to the source process, but enumerates and resolves
    // the destination's exact received wire value, never the observer's codec.
    let inspector_handle = source.create_object(
        crate::diagnostics::inspect::ObjectInspector::try_system(domain)
            .map_err(|_| Error::Construction)?,
        Rights::INSPECT.union(Rights::INSPECT_DETAILS),
    )?;
    let inspector = source.resolve_handle::<crate::diagnostics::inspect::ObjectInspector>(
        inspector_handle,
        Rights::INSPECT.union(Rights::INSPECT_DETAILS),
    )?;
    let observation = inspector
        .object()
        .scan_process_handles(destination.koid().get(), 0)
        .map_err(|_| Error::State(100))?;
    let observed = observation
        .entries()
        .find(|entry| entry.handle.value == received)
        .ok_or(Error::State(101))?;
    if observed.process_koid != destination.koid()
        || handle_wire::to_abi(observed.handle.value) != read_u64(&received_record, 0)?
        || observed.handle.info != destination.handle_info(received, Rights::NONE)?
    {
        return Err(Error::State(102));
    }
    let details = inspector
        .object()
        .read_details(destination.koid().get(), handle_wire::to_abi(receiver), 0)
        .map_err(|_| Error::State(103))?;
    if details.koid != destination.handle_info(receiver, Rights::NONE)?.koid {
        return Err(Error::State(104));
    }
    source.close_handle(inspector_handle)?;
    drop(inspector);
    source.close_handle(sender)?;
    destination.close_handle(receiver)?;
    destination.close_handle(received)?;
    let report = destination.request_stop(TerminalReason::Requested);
    if !report.newly_requested || !report.dispatch_complete {
        return Err(Error::State(30));
    }
    retire_process(&destination)
}

extern "C" fn capability_receive_worker(context: usize) {
    // SAFETY: the test passes one live boxed `CapabilityReceiveContext` and
    // retains it until this worker has fully left the scheduler.
    let context =
        unsafe { &*core::ptr::with_exposed_provenance::<CapabilityReceiveContext>(context) };
    let outcome = context
        .process
        .resolve_invocation(context.endpoint)
        .map_err(CapabilityChannelServiceError::from)
        .and_then(|receiver| {
            capability_channel_receive(
                &context.process,
                &receiver,
                hyper::abi::native::HYPER_NATIVE_DEADLINE_INFINITE,
                Some(context.byte_output),
                Some(context.slot_output),
                || false,
            )
        });
    let status = match outcome {
        Ok(CapabilityReceiveOutcome::Delivered(CapabilityDeliveryInfo { bytes, handles: 1 }))
            if bytes == CAPABILITY_MESSAGE.len() =>
        {
            1
        }
        _ => 2,
    };
    context.status.store(status, Ordering::Release);
}

fn send_disposition(value: HandleValue, rights: Rights) -> [u8; CAPABILITY_RECORD_BYTES] {
    let mut record = [0; CAPABILITY_RECORD_BYTES];
    record[0..8].copy_from_slice(&handle_wire::to_abi(value).to_ne_bytes());
    record[8..16].copy_from_slice(&rights.bits().to_ne_bytes());
    record[16..20].copy_from_slice(&hyper::abi::native::HYPER_NATIVE_OBJECT_EVENT.to_ne_bytes());
    record[20..24].copy_from_slice(
        &(hyper::abi::native::HYPER_NATIVE_CAPABILITY_DISPOSITION_MOVE as u32).to_ne_bytes(),
    );
    record
}

fn receive_slot(rights: Rights) -> [u8; CAPABILITY_RECORD_BYTES] {
    let mut record = [0; CAPABILITY_RECORD_BYTES];
    record[8..16].copy_from_slice(&rights.bits().to_ne_bytes());
    record[16..20].copy_from_slice(&hyper::abi::native::HYPER_NATIVE_OBJECT_EVENT.to_ne_bytes());
    record
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64, Error> {
    let value = bytes
        .get(offset..offset + core::mem::size_of::<u64>())
        .and_then(|value| value.try_into().ok())
        .ok_or(Error::Construction)?;
    Ok(u64::from_ne_bytes(value))
}

fn verify_handle_accounting_churn(process: &Process) -> Result<(), Error> {
    let baseline = process
        .resource_domain()
        .usage()
        .total(ResourceKind::Handles);
    let event = Event::try_new(&process.resource_domain()).map_err(|_| Error::Construction)?;
    let original = process.create_object(event, <Event as KernelObject>::SUPPORTED_RIGHTS)?;
    let mut handles = alloc::vec::Vec::new();
    handles
        .try_reserve_exact(257)
        .map_err(|_| Error::Construction)?;
    for _ in 0..257 {
        handles.push(process.duplicate_handle(original, Rights::WAIT)?);
    }
    process.close_handle(original)?;
    for stale in handles {
        let replacement = process.replace_handle(stale, Rights::WAIT)?;
        if process.handle_info(stale, Rights::NONE).is_ok() {
            return Err(Error::State(1));
        }
        process.close_handle(replacement)?;
    }
    if process
        .resource_domain()
        .usage()
        .total(ResourceKind::Handles)
        != baseline
    {
        return Err(Error::State(2));
    }
    Ok(())
}

/// Multiple entries share a record: removing one must not unlink its sibling.
/// Exercise middle, head and tail unlink plus rekey after partial removal through
/// real Process publication/close APIs rather than a duplicate accounting model.
fn verify_partial_charge_records(process: &Process) -> Result<(), Error> {
    let domain = process.resource_domain();
    let baseline = domain.usage().total(ResourceKind::Handles);
    let oldest = byte_channel_create(process)?;
    let middle = byte_channel_create(process)?;
    let newest = byte_channel_create(process)?;
    process.close_handle(middle[0])?;
    let replacement = process.replace_handle(middle[1], Rights::READ)?;
    if process.handle_info(middle[1], Rights::NONE).is_ok()
        || domain.usage().total(ResourceKind::Handles) != baseline + 5
    {
        return Err(Error::State(40));
    }
    process.close_handle(replacement)?;
    process.close_handle(newest[1])?;
    process.close_handle(oldest[0])?;
    // Both surviving entries must still have their charge/index association.
    process.handle_info(oldest[1], Rights::NONE)?;
    process.handle_info(newest[0], Rights::NONE)?;
    if domain.usage().total(ResourceKind::Handles) != baseline + 2 {
        return Err(Error::State(41));
    }
    process.close_handle(newest[0])?;
    process.close_handle(oldest[1])?;
    if domain.usage().total(ResourceKind::Handles) != baseline {
        return Err(Error::State(42));
    }
    Ok(())
}

fn verify_handle_quota_rollback(
    process: &Process,
    original_limits: ResourceLimits,
) -> Result<(), Error> {
    let domain = process.resource_domain();
    let baseline = domain.usage().total(ResourceKind::Handles);
    domain
        .set_local_limits(original_limits.with(ResourceKind::Handles, baseline + 1))
        .map_err(|_| Error::Construction)?;
    // The first per-handle reservation succeeds, then the second fails. Both
    // unpublished slots and the first charge must be returned by rollback.
    let result = process.reserve_handles::<2>();
    domain
        .set_local_limits(original_limits)
        .map_err(|_| Error::Construction)?;
    let rejected = match result {
        Ok(reservation) => {
            process.abort_handles(reservation);
            false
        }
        Err(ProcessError::Resource(_)) => true,
        Err(error) => return Err(error.into()),
    };
    if !rejected || domain.usage().total(ResourceKind::Handles) != baseline {
        return Err(Error::State(43));
    }
    let reservation = process.reserve_handles::<2>()?;
    process.abort_handles(reservation);
    if domain.usage().total(ResourceKind::Handles) != baseline {
        return Err(Error::State(44));
    }
    Ok(())
}

fn verify_handle_admission_after_stop(
    domain: &ResourceDomain,
    group: &TaskGroup,
) -> Result<(), Error> {
    let baseline = domain.usage().total(ResourceKind::Handles);
    let process = create_process(domain, group)?;
    let inspector = crate::diagnostics::inspect::ObjectInspector::try_system(domain)
        .map_err(|_| Error::Construction)?;
    inspector
        .scan_process_handles(process.koid().get(), 0)
        .map_err(|_| Error::State(48))?;
    let object =
        ObjectPublication::try_new(Event::try_new(domain).map_err(|_| Error::Construction)?)
            .map_err(|_| Error::Construction)?;
    let handle = PreparedHandle::try_from_new_object(object, Rights::WAIT, HandleFlags::NONE)
        .map_err(ProcessError::from)?;
    let reservation = process.reserve_handles::<1>()?;
    if process.requested_stop_reason().is_some() {
        return Err(Error::State(45));
    }
    process.request_stop(TerminalReason::Requested);
    process.request_stop(TerminalReason::ProcessExited { status: 99 });
    if process.requested_stop_reason() != Some(TerminalReason::Requested) {
        return Err(Error::State(45));
    }
    // The reservation was admitted before stop, but ordinary publication must
    // now fail reversibly. Consume-on-success transactions have separate rules.
    match process.publish_handles(reservation, [handle]) {
        Ok(_) => return Err(Error::State(45)),
        Err(failure) => {
            if !matches!(failure.error, ProcessError::Lifecycle(_)) {
                return Err(Error::State(46));
            }
            drop(failure);
        }
    }
    retire_process(&process)?;
    // Retain the Process so lookup still finds its identity after its handle
    // table has retired. Inspection must report disappearance, not BAD_STATE.
    if !matches!(
        process.scan_handles(crate::capability::HandleScanCursor::from_token(0)),
        Err(ProcessError::Handle(HandleError::TableRetired))
    ) || !matches!(
        inspector.scan_process_handles(process.koid().get(), 0),
        Err(crate::diagnostics::inspect::Error::NotFound)
    ) {
        return Err(Error::State(49));
    }
    if domain.usage().total(ResourceKind::Handles) != baseline {
        return Err(Error::State(47));
    }
    Ok(())
}

/// Exercises the transaction used by consume-on-success lifecycle syscalls.
///
/// An observer interleaved after the source claim must see neither the claimed
/// source nor a prematurely published destination. Rollback must restore the
/// exact source generation, while commit must make the source stale and the
/// replacement live in one table mutation.
fn verify_atomic_handle_replacement(process: &Process) -> Result<(), Error> {
    let rights = Rights::WAIT.union(Rights::INSPECT);
    let source = process.create_object(
        Event::try_new(&process.resource_domain()).map_err(|_| Error::Construction)?,
        rights,
    )?;
    let source_info = process.handle_info(source, Rights::NONE)?;
    let reservation = process.reserve_handles::<1>()?;
    let consumption = process.prepare_handle_consumption(
        source,
        Rights::WAIT,
        source_info.kind,
        source_info.koid,
    )?;
    if !matches!(
        process.handle_info(source, Rights::NONE),
        Err(ProcessError::Handle(HandleError::Busy))
    ) {
        consumption.rollback();
        process.abort_handles(reservation);
        return Err(Error::State(31));
    }
    consumption.rollback();
    process.abort_handles(reservation);
    if process.handle_info(source, rights)?.koid != source_info.koid {
        return Err(Error::State(32));
    }

    let reservation = process.reserve_handles::<1>()?;
    let consumption = process.prepare_handle_consumption(
        source,
        Rights::WAIT,
        source_info.kind,
        source_info.koid,
    )?;
    let replacement = ObjectPublication::try_new(
        Event::try_new(&process.resource_domain()).map_err(|_| Error::Construction)?,
    )
    .map_err(|_| Error::Construction)?;
    let replacement = PreparedHandle::try_from_new_object(replacement, rights, HandleFlags::NONE)
        .map_err(ProcessError::from)?;
    let replacement = consumption.commit_replacement(reservation, [replacement])[0];
    if process.handle_info(source, Rights::NONE).is_ok() {
        process.close_handle(replacement)?;
        return Err(Error::State(33));
    }
    if process.handle_info(replacement, rights)?.koid == source_info.koid {
        process.close_handle(replacement)?;
        return Err(Error::State(34));
    }
    process.close_handle(replacement)?;
    Ok(())
}

fn verify_fifo_and_buffer_contract(process: &Process) -> Result<(), Error> {
    let [sender, receiver] = byte_channel_create(process)?;
    let first = write_user(process, 0, b"first")?;
    let second = write_user(process, 0x40, b"second")?;
    byte_channel_write(process, &process.resolve_invocation(sender)?, Some(first))?;
    byte_channel_write(process, &process.resolve_invocation(sender)?, Some(second))?;

    let short = scratch(0x100, 2)?;
    process.copy_to_user(short, &[0x5a, 0xa5])?;
    if byte_channel_read(process, &process.resolve_invocation(receiver)?, Some(short))?
        != (ByteChannelReadOutcome::BufferTooSmall { bytes: 5 })
    {
        return Err(Error::State(3));
    }
    let mut sentinel = [0; 2];
    process.copy_from_user(short, &mut sentinel)?;
    if sentinel != [0x5a, 0xa5] {
        return Err(Error::State(4));
    }

    verify_read(process, receiver, 0x140, b"first", 5)?;
    verify_read(process, receiver, 0x180, b"second", 6)?;
    if !matches!(
        byte_channel_read(process, &process.resolve_invocation(receiver)?, None),
        Err(ByteChannelServiceError::Channel(
            ByteChannelError::WouldBlock
        ))
    ) {
        return Err(Error::State(7));
    }
    process.close_handle(sender)?;
    process.close_handle(receiver)?;
    Ok(())
}

fn verify_copy_failure_restores_message(process: &Process) -> Result<(), Error> {
    let [sender, receiver] = byte_channel_create(process)?;
    let source = write_user(process, 0x200, b"atomic")?;
    byte_channel_write(process, &process.resolve_invocation(sender)?, Some(source))?;
    let output = scratch(0x240, 8)?;

    fail_exposed_write_after_copy_for_test(1);
    if !matches!(
        byte_channel_read(
            process,
            &process.resolve_invocation(receiver)?,
            Some(output)
        ),
        Err(ByteChannelServiceError::Process(ProcessError::UserMemory(
            _
        )))
    ) {
        return Err(Error::State(8));
    }
    verify_read(process, receiver, 0x240, b"atomic", 9)?;
    process.close_handle(sender)?;
    process.close_handle(receiver)?;
    Ok(())
}

fn verify_read(
    process: &Process,
    receiver: HandleValue,
    offset: u64,
    expected: &[u8],
    stage: usize,
) -> Result<(), Error> {
    let output = scratch(offset, 16)?;
    if byte_channel_read(
        process,
        &process.resolve_invocation(receiver)?,
        Some(output),
    )? != (ByteChannelReadOutcome::Received {
        bytes: expected.len() as u64,
    }) {
        return Err(Error::State(stage));
    }
    let mut actual = [0; 16];
    let target = actual
        .get_mut(..expected.len())
        .ok_or(Error::State(stage))?;
    process.copy_from_user(
        UserSlice::new(output.base(), expected.len() as u64).map_err(|_| Error::Construction)?,
        target,
    )?;
    if target != expected {
        return Err(Error::State(stage));
    }
    Ok(())
}

fn create_process(domain: &ResourceDomain, group: &TaskGroup) -> Result<Process, Error> {
    let code_range = scratch_image(0, PAGE_SIZE)?;
    let stack_range = scratch_image(PAGE_SIZE * 2, PAGE_SIZE)?;
    // Two empty pages are reserved for child-VMAR capability attenuation probes.
    let image_range = scratch_image(0, PAGE_SIZE * 5)?;
    let address_space = prepare_native_entry_self_test(
        domain.clone(),
        image_range,
        code_range,
        stack_range,
        &TEST_CODE,
    )
    .map_err(|_| Error::AddressSpace)?;
    let image = ProcessImage::try_native(
        MachineAbi::Aarch64,
        code_range.base(),
        stack_range.end(),
        UserAddress::new(0),
    )
    .map_err(|_| Error::Construction)?;
    let prepared =
        match PreparedProcess::try_new(image, group.clone(), domain.clone(), address_space) {
            Ok(prepared) => prepared,
            Err(failure) => {
                let (_, address_space) = failure.into_parts();
                crate::mm::user_space::NativeAddressSpace::retire(address_space)
                    .map_err(|_| Error::AddressSpace)?;
                return Err(Error::Construction);
            }
        };
    let object = ProcessObject::try_service(prepared.process()).map_err(|_| Error::Construction)?;
    Ok(prepared.publish(
        object,
        crate::process::ProcessNameSnapshot::from_validated("channel-test"),
    ))
}

fn write_user(process: &Process, offset: u64, bytes: &[u8]) -> Result<UserSlice, Error> {
    let range = scratch(offset, bytes.len() as u64)?;
    process.copy_to_user(range, bytes)?;
    Ok(range)
}

fn scratch(offset: u64, length: u64) -> Result<UserSlice, Error> {
    if offset.checked_add(length).is_none_or(|end| end > PAGE_SIZE) {
        return Err(Error::Construction);
    }
    UserSlice::new(UserAddress::new(SCRATCH_BASE + offset), length).map_err(|_| Error::Construction)
}

fn scratch_image(offset: u64, length: u64) -> Result<UserSlice, Error> {
    UserSlice::new(UserAddress::new(IMAGE_BASE + offset), length).map_err(|_| Error::Construction)
}

fn retire_process(process: &Process) -> Result<(), Error> {
    if crate::task::wait_for_test_progress(crate::task::TEST_PROGRESS_TIMEOUT_NS, || {
        Ok::<_, Error>(process.snapshot().phase == ProcessPhase::Retired)
    })? {
        Ok(())
    } else {
        Err(Error::Construction)
    }
}
