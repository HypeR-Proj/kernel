// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Process-facing `ByteChannel` transactions through real native mappings.

use alloc::boxed::Box;
use core::sync::atomic::{AtomicUsize, Ordering};

use hyper::mm::PAGE_SIZE;

use crate::kernel::accounting::{ResourceDomain, ResourceKind, ResourceLimits};
use crate::kernel::capability::{HandleValue, Rights};
use crate::kernel::ipc::{
    ByteChannelError, ByteChannelReadOutcome, ByteChannelServiceError, CapabilityChannel,
    CapabilityChannelServiceError, CapabilityDeliveryInfo, CapabilityReceiveOutcome,
    byte_channel_create, byte_channel_read, byte_channel_write, capability_channel_receive,
    capability_channel_try_send,
};
use crate::kernel::mm::user_space::{
    UserAddress, UserSlice, fail_exposed_write_after_copy_for_test, prepare_native_entry_self_test,
};
use crate::kernel::object::{Event, KernelObject};
use crate::kernel::process::{
    MachineAbi, PreparedProcess, Process, ProcessError, ProcessImage, ProcessObject, ProcessPhase,
    TaskGroup, TerminalReason,
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
    Scheduler(crate::kernel::task::scheduler::Error),
    Service(ByteChannelServiceError),
    Sleep(crate::kernel::task::SleepError),
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

impl From<crate::kernel::task::scheduler::Error> for Error {
    fn from(error: crate::kernel::task::scheduler::Error) -> Self {
        Self::Scheduler(error)
    }
}

impl From<crate::kernel::task::SleepError> for Error {
    fn from(error: crate::kernel::task::SleepError) -> Self {
        Self::Sleep(error)
    }
}

pub(super) fn run() -> Result<(), Error> {
    let domain =
        ResourceDomain::try_new_root(ResourceLimits::UNLIMITED).map_err(|_| Error::Construction)?;
    let group = TaskGroup::try_new(&domain).map_err(|_| Error::Group)?;
    let process = create_process(&domain, &group)?;

    verify_handle_accounting_churn(&process)?;
    verify_fifo_and_buffer_contract(&process)?;
    verify_copy_failure_restores_message(&process)?;
    verify_cross_process_capability_transfer(&process, &domain, &group)?;

    let retained_space = process.address_space_owner()?;
    let report = process.request_stop(TerminalReason::Requested);
    if !report.newly_requested || !report.dispatch_complete {
        return Err(Error::State(20));
    }
    crate::kernel::task::sleep_ms(20)?;
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

    let context = Box::new(CapabilityReceiveContext {
        process: destination.clone(),
        endpoint: receiver,
        byte_output: destination_bytes,
        slot_output: destination_record,
        status: AtomicUsize::new(0),
    });
    let context = Box::into_raw(context);
    let worker = match crate::kernel::task::scheduler::kthread_create(
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
    crate::kernel::task::scheduler::thread_ready(worker)?;

    let sender_object = source.resolve_handle::<CapabilityChannel>(sender, Rights::WRITE)?;
    if !crate::kernel::task::wait_for_test_progress(
        crate::kernel::task::TEST_PROGRESS_TIMEOUT_NS,
        || {
            Ok::<_, Error>(
                sender_object.object().signal_level_for_test()
                    & CapabilityChannel::PEER_RECEIVING.bits()
                    != 0,
            )
        },
    )? {
        return Err(Error::State(23));
    }
    capability_channel_try_send(source, sender, Some(source_bytes), Some(source_record))?;

    // SAFETY: the allocation remains owned by this test until worker
    // quiescence, and all shared observations use atomics or immutable fields.
    let receive_status = unsafe { &(*context).status };
    if !crate::kernel::task::wait_for_test_progress(
        crate::kernel::task::TEST_PROGRESS_TIMEOUT_NS,
        || Ok::<_, Error>(receive_status.load(Ordering::Acquire) != 0),
    )? {
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
        HandleValue::try_from_raw(read_u64(&received_record, 0)?).map_err(|_| Error::State(28))?;
    let info = destination.handle_info(received, transferred_rights)?;
    if info.kind.get() != hyper::abi::native::HYPER_NATIVE_OBJECT_EVENT
        || info.rights != transferred_rights
    {
        return Err(Error::State(29));
    }

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
    let outcome = capability_channel_receive(
        &context.process,
        context.endpoint,
        hyper::abi::native::HYPER_NATIVE_DEADLINE_INFINITE,
        Some(context.byte_output),
        Some(context.slot_output),
        || false,
    );
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
    record[0..8].copy_from_slice(&value.get().to_ne_bytes());
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

fn verify_fifo_and_buffer_contract(process: &Process) -> Result<(), Error> {
    let [sender, receiver] = byte_channel_create(process)?;
    let first = write_user(process, 0, b"first")?;
    let second = write_user(process, 0x40, b"second")?;
    byte_channel_write(process, sender, Some(first))?;
    byte_channel_write(process, sender, Some(second))?;

    let short = scratch(0x100, 2)?;
    process.copy_to_user(short, &[0x5a, 0xa5])?;
    if byte_channel_read(process, receiver, Some(short))?
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
        byte_channel_read(process, receiver, None),
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
    byte_channel_write(process, sender, Some(source))?;
    let output = scratch(0x240, 8)?;

    fail_exposed_write_after_copy_for_test(1);
    if !matches!(
        byte_channel_read(process, receiver, Some(output)),
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
    if byte_channel_read(process, receiver, Some(output))?
        != (ByteChannelReadOutcome::Received {
            bytes: expected.len() as u64,
        })
    {
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
    let image_range = scratch_image(0, PAGE_SIZE * 3)?;
    let pin = crate::kernel::task::scheduler::preempt_disable().map_err(|_| Error::Construction)?;
    let address_space = prepare_native_entry_self_test(
        domain.clone(),
        image_range,
        code_range,
        stack_range,
        &TEST_CODE,
        &pin,
    );
    crate::kernel::task::scheduler::preempt_enable_and_reschedule(pin)
        .map_err(|_| Error::Construction)?;
    let address_space = address_space.map_err(|_| Error::AddressSpace)?;
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
                crate::kernel::mm::user_space::NativeAddressSpace::retire(address_space)
                    .map_err(|_| Error::AddressSpace)?;
                return Err(Error::Construction);
            }
        };
    let object = ProcessObject::try_service(prepared.process()).map_err(|_| Error::Construction)?;
    Ok(prepared.publish(
        object,
        crate::kernel::process::ProcessNameSnapshot::from_validated("channel-test"),
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
    if crate::kernel::task::wait_for_test_progress(
        crate::kernel::task::TEST_PROGRESS_TIMEOUT_NS,
        || Ok::<_, Error>(process.snapshot().phase == ProcessPhase::Retired),
    )? {
        Ok(())
    } else {
        Err(Error::Construction)
    }
}
