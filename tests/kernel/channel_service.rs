// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Process-facing Channel transactions through real native user mappings.

use hyper::mm::PAGE_SIZE;

use crate::kernel::accounting::{ResourceDomain, ResourceKind, ResourceLimits};
use crate::kernel::capability::{HandleError, HandleValue, Rights};
use crate::kernel::ipc::{
    ChannelError, ChannelReadOutcome, ChannelServiceError, ReadBuffers, channel_create,
    channel_read, channel_write,
};
use crate::kernel::mm::user_space::{
    UserAddress, UserSlice, fail_exposed_write_after_copy_for_test, prepare_native_entry_self_test,
};
use crate::kernel::object::{Event, KernelObject};
use crate::kernel::process::{
    MachineAbi, PreparedProcess, Process, ProcessError, ProcessImage, ProcessPhase, TaskGroup,
    TerminalReason,
};

const IMAGE_BASE: u64 = 0x80_0000;
const SCRATCH_BASE: u64 = IMAGE_BASE + PAGE_SIZE * 2;
const TEST_CODE: [u8; 4] = [0x00, 0x00, 0x20, 0xd4];

pub(super) enum Error {
    AddressSpace,
    Construction,
    Group,
    Process(ProcessError),
    Service(ChannelServiceError),
    Sleep(crate::kernel::task::SleepError),
    State(usize),
}

impl core::fmt::Debug for Error {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::AddressSpace => formatter.write_str("AddressSpace"),
            Self::Construction => formatter.write_str("Construction"),
            Self::Group => formatter.write_str("Group"),
            Self::Process(error) => formatter.debug_tuple("Process").field(error).finish(),
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

impl From<ChannelServiceError> for Error {
    fn from(error: ChannelServiceError) -> Self {
        Self::Service(error)
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
    verify_create_and_fifo(&process)?;
    verify_buffer_too_small_and_event_transfer(&process)?;
    verify_invalid_and_unsupported_transfer_rollback(&process)?;
    verify_copy_failure_restores_message_without_authority(&process)?;
    verify_late_copy_failure_restores_complete_message(&process)?;

    let retained_space = process.address_space_owner()?;
    let retry_cpu = if crate::kernel::cpu::online_cpu_count() > 1 {
        let cpu = hyper::cpu::CpuIndex::new(1).ok_or(Error::Construction)?;
        crate::kernel::reaper::set_affinity_for_test(
            crate::kernel::task::scheduler::CpuMask::single(cpu),
        )
        .map_err(|_| Error::Construction)?;
        Some(cpu)
    } else {
        None
    };
    let retirement = (|| {
        let report = process.request_stop(TerminalReason::Requested);
        if !report.newly_requested || !report.dispatch_complete {
            return Err(Error::State(90));
        }
        crate::kernel::task::sleep_ms(20).map_err(|_| Error::Construction)?;
        if process.snapshot().phase != ProcessPhase::Retiring {
            return Err(Error::State(91));
        }
        if let Some(cpu) = retry_cpu
            && !crate::kernel::task::wait_for_test_progress(
                crate::kernel::task::TEST_PROGRESS_TIMEOUT_NS,
                || Ok::<_, Error>(crate::kernel::reaper::retry_observed_on_for_test(cpu)),
            )?
        {
            return Err(Error::State(95));
        }
        drop(retained_space);
        retire_process(&process)
    })();
    let affinity_restoration = if retry_cpu.is_some() {
        crate::kernel::reaper::set_affinity_for_test(crate::kernel::task::scheduler::CpuMask::ALL)
            .map_err(|_| Error::Construction)
    } else {
        Ok(())
    };
    retirement?;
    affinity_restoration?;
    if domain.usage().total(ResourceKind::Handles) != 0 {
        return Err(Error::State(92));
    }
    group.request_stop().map_err(|_| Error::Group)?;
    group.finish_retirement().map_err(|_| Error::Group)?;
    Ok(())
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
    // Remove the oldest record first, then interleave slot reuse and replacement
    // across several segment boundaries. Stale generations must stay invalid.
    process.close_handle(original)?;
    for stale in handles {
        let replacement = process.replace_handle(stale, Rights::WAIT)?;
        if process.handle_info(stale, Rights::NONE).is_ok() {
            return Err(Error::State(93));
        }
        process.close_handle(replacement)?;
        let event = Event::try_new(&process.resource_domain()).map_err(|_| Error::Construction)?;
        let reused = process.create_object(event, <Event as KernelObject>::SUPPORTED_RIGHTS)?;
        process.close_handle(reused)?;
    }
    if process
        .resource_domain()
        .usage()
        .total(ResourceKind::Handles)
        != baseline
    {
        return Err(Error::State(94));
    }
    Ok(())
}

fn verify_create_and_fifo(process: &Process) -> Result<(), Error> {
    let [sender, receiver] = channel_create(process)?;
    if sender == receiver
        || process.handle_info(sender, Rights::WRITE).is_err()
        || process.handle_info(receiver, Rights::READ).is_err()
    {
        return Err(Error::State(1));
    }

    let first = write_user(process, 0x000, b"first")?;
    channel_write(process, sender, Some(first), None, 0)?;
    let second = write_user(process, 0x040, b"second")?;
    channel_write(process, sender, Some(second), None, 0)?;

    let output = scratch(0x100, 32)?;
    verify_read_bytes(process, receiver, output, b"first", 2)?;
    verify_read_bytes(process, receiver, output, b"second", 3)?;
    if !matches!(
        channel_read(
            process,
            receiver,
            ReadBuffers {
                bytes: Some(output),
                handles: None,
            },
        ),
        Err(ChannelServiceError::Channel(ChannelError::WouldBlock))
    ) {
        return Err(Error::State(4));
    }
    Ok(())
}

fn verify_buffer_too_small_and_event_transfer(process: &Process) -> Result<(), Error> {
    let [sender, receiver] = channel_create(process)?;
    let event = create_event(process)?;
    let event_koid = process
        .resolve_handle::<Event>(event, Rights::INSPECT)?
        .koid();
    let disposition = encode_disposition(
        event,
        hyper::abi::native::HYPER_NATIVE_CHANNEL_DISPOSITION_SAME_RIGHTS,
        hyper::abi::native::HYPER_NATIVE_OBJECT_EVENT,
        0,
    );
    let disposition = write_user(process, 0x180, &disposition)?;
    let payload = write_user(process, 0x1c0, b"cap")?;
    channel_write(process, sender, Some(payload), Some(disposition), 1)?;
    verify_invalid_handle(process, event, 10)?;

    let byte_output = scratch(0x200, 2)?;
    let sentinel = [0x5a, 0xa5];
    process.copy_to_user(byte_output, &sentinel)?;
    let outcome = channel_read(
        process,
        receiver,
        ReadBuffers {
            bytes: Some(byte_output),
            handles: None,
        },
    )?;
    if outcome
        != (ChannelReadOutcome::BufferTooSmall {
            bytes: 3,
            handles: 1,
        })
    {
        return Err(Error::State(11));
    }
    let mut unchanged = [0; 2];
    process.copy_from_user(byte_output, &mut unchanged)?;
    if unchanged != sentinel {
        return Err(Error::State(12));
    }

    let byte_output = scratch(0x240, 8)?;
    let handle_output = scratch(0x280, 8)?;
    let outcome = channel_read(
        process,
        receiver,
        ReadBuffers {
            bytes: Some(byte_output),
            handles: Some(handle_output),
        },
    )?;
    if outcome
        != (ChannelReadOutcome::Received {
            bytes: 3,
            handles: 1,
        })
    {
        return Err(Error::State(13));
    }
    let mut bytes = [0; 3];
    process.copy_from_user(scratch(0x240, 3)?, &mut bytes)?;
    if bytes != *b"cap" {
        return Err(Error::State(14));
    }
    let received = read_handle(process, handle_output)?;
    let received_event = process.resolve_handle::<Event>(received, Rights::SIGNAL)?;
    if received_event.koid() != event_koid {
        return Err(Error::State(15));
    }
    Ok(())
}

fn verify_invalid_and_unsupported_transfer_rollback(process: &Process) -> Result<(), Error> {
    let [invalid_sender, invalid_receiver] = channel_create(process)?;
    let event = create_event(process)?;
    let invalid = encode_disposition(
        event,
        hyper::abi::native::HYPER_NATIVE_CHANNEL_DISPOSITION_SAME_RIGHTS,
        hyper::abi::native::HYPER_NATIVE_OBJECT_EVENT,
        1,
    );
    let invalid = write_user(process, 0x300, &invalid)?;
    if !matches!(
        channel_write(process, invalid_sender, None, Some(invalid), 1),
        Err(ChannelServiceError::InvalidDisposition)
    ) {
        return Err(Error::State(20));
    }
    if process
        .resolve_handle::<Event>(event, Rights::TRANSFER)
        .is_err()
    {
        return Err(Error::State(21));
    }
    verify_empty(process, invalid_receiver, 22)?;

    let prefix_event = create_event(process)?;
    let stale = create_event(process)?;
    process.close_handle(stale)?;
    let invalid_batch = encode_two_dispositions(
        encode_disposition(
            prefix_event,
            hyper::abi::native::HYPER_NATIVE_CHANNEL_DISPOSITION_SAME_RIGHTS,
            hyper::abi::native::HYPER_NATIVE_OBJECT_EVENT,
            0,
        ),
        encode_disposition(
            stale,
            hyper::abi::native::HYPER_NATIVE_CHANNEL_DISPOSITION_SAME_RIGHTS,
            hyper::abi::native::HYPER_NATIVE_OBJECT_EVENT,
            0,
        ),
    );
    let invalid_batch = write_user(process, 0x340, &invalid_batch)?;
    if !matches!(
        channel_write(process, invalid_sender, None, Some(invalid_batch), 2,),
        Err(ChannelServiceError::Process(ProcessError::Handle(
            HandleError::InvalidHandle
        )))
    ) {
        return Err(Error::State(23));
    }
    if process
        .resolve_handle::<Event>(prefix_event, Rights::TRANSFER)
        .is_err()
    {
        return Err(Error::State(24));
    }
    verify_empty(process, invalid_receiver, 25)?;

    let [sender, receiver] = channel_create(process)?;
    let [channel_source, _channel_peer] = channel_create(process)?;
    let prefix_event = create_event(process)?;
    let unsupported = encode_two_dispositions(
        encode_disposition(
            prefix_event,
            hyper::abi::native::HYPER_NATIVE_CHANNEL_DISPOSITION_SAME_RIGHTS,
            hyper::abi::native::HYPER_NATIVE_OBJECT_EVENT,
            0,
        ),
        encode_disposition(
            channel_source,
            hyper::abi::native::HYPER_NATIVE_CHANNEL_DISPOSITION_SAME_RIGHTS,
            hyper::abi::native::HYPER_NATIVE_OBJECT_CHANNEL,
            0,
        ),
    );
    let unsupported = write_user(process, 0x380, &unsupported)?;
    if !matches!(
        channel_write(process, sender, None, Some(unsupported), 2),
        Err(ChannelServiceError::Process(ProcessError::Handle(
            HandleError::UnsupportedTransfer
        )))
    ) {
        return Err(Error::State(26));
    }
    if process
        .resolve_handle::<Event>(prefix_event, Rights::TRANSFER)
        .is_err()
    {
        return Err(Error::State(27));
    }
    if process
        .resolve_handle::<crate::kernel::ipc::ChannelEndpoint>(channel_source, Rights::TRANSFER)
        .is_err()
    {
        return Err(Error::State(28));
    }
    verify_empty(process, receiver, 29)
}

fn verify_copy_failure_restores_message_without_authority(process: &Process) -> Result<(), Error> {
    let [sender, receiver] = channel_create(process)?;
    let event = create_event(process)?;
    let event_koid = process
        .resolve_handle::<Event>(event, Rights::INSPECT)?
        .koid();
    let disposition = encode_disposition(
        event,
        hyper::abi::native::HYPER_NATIVE_CHANNEL_DISPOSITION_SAME_RIGHTS,
        hyper::abi::native::HYPER_NATIVE_OBJECT_EVENT,
        0,
    );
    let disposition = write_user(process, 0x380, &disposition)?;
    channel_write(process, sender, None, Some(disposition), 1)?;
    verify_invalid_handle(process, event, 30)?;

    let handle_output = scratch(0x3c0, 8)?;
    process.copy_to_user(handle_output, &[0xa5; 8])?;
    fail_exposed_write_after_copy_for_test(1);
    if !matches!(
        channel_read(
            process,
            receiver,
            ReadBuffers {
                bytes: None,
                handles: Some(handle_output),
            },
        ),
        Err(ChannelServiceError::Process(ProcessError::UserMemory(_)))
    ) {
        return Err(Error::State(31));
    }

    // The backend reports failure after copying the reserved future value.
    // That value must remain unresolvable until a later receive commit wins.
    let provisional = read_handle(process, handle_output)?;
    verify_invalid_handle(process, provisional, 32)?;

    let outcome = channel_read(
        process,
        receiver,
        ReadBuffers {
            bytes: None,
            handles: Some(handle_output),
        },
    )?;
    if outcome
        != (ChannelReadOutcome::Received {
            bytes: 0,
            handles: 1,
        })
    {
        return Err(Error::State(33));
    }
    let received = read_handle(process, handle_output)?;
    let received_event = process.resolve_handle::<Event>(received, Rights::SIGNAL)?;
    if received_event.koid() != event_koid {
        return Err(Error::State(34));
    }
    Ok(())
}

fn verify_late_copy_failure_restores_complete_message(process: &Process) -> Result<(), Error> {
    let [sender, receiver] = channel_create(process)?;
    let event = create_event(process)?;
    let event_koid = process
        .resolve_handle::<Event>(event, Rights::INSPECT)?
        .koid();
    let disposition = encode_disposition(
        event,
        hyper::abi::native::HYPER_NATIVE_CHANNEL_DISPOSITION_SAME_RIGHTS,
        hyper::abi::native::HYPER_NATIVE_OBJECT_EVENT,
        0,
    );
    let disposition = write_user(process, 0x400, &disposition)?;
    let payload = write_user(process, 0x440, b"atomic")?;
    channel_write(process, sender, Some(payload), Some(disposition), 1)?;
    verify_invalid_handle(process, event, 40)?;

    let byte_output = scratch(0x480, 8)?;
    let handle_output = scratch(0x4c0, 8)?;
    process.copy_to_user(byte_output, &[0x5a; 8])?;
    process.copy_to_user(handle_output, &[0xa5; 8])?;
    fail_exposed_write_after_copy_for_test(2);
    if !matches!(
        channel_read(
            process,
            receiver,
            ReadBuffers {
                bytes: Some(byte_output),
                handles: Some(handle_output),
            },
        ),
        Err(ChannelServiceError::Process(ProcessError::UserMemory(_)))
    ) {
        return Err(Error::State(41));
    }

    let mut partially_visible = [0; 6];
    process.copy_from_user(scratch(0x480, 6)?, &mut partially_visible)?;
    if partially_visible != *b"atomic" {
        return Err(Error::State(42));
    }
    let provisional = read_handle(process, handle_output)?;
    verify_invalid_handle(process, provisional, 43)?;

    let outcome = channel_read(
        process,
        receiver,
        ReadBuffers {
            bytes: Some(byte_output),
            handles: Some(handle_output),
        },
    )?;
    if outcome
        != (ChannelReadOutcome::Received {
            bytes: 6,
            handles: 1,
        })
    {
        return Err(Error::State(44));
    }
    let mut bytes = [0; 6];
    process.copy_from_user(scratch(0x480, 6)?, &mut bytes)?;
    if bytes != *b"atomic" {
        return Err(Error::State(45));
    }
    let received = read_handle(process, handle_output)?;
    let received_event = process.resolve_handle::<Event>(received, Rights::SIGNAL)?;
    if received_event.koid() != event_koid {
        return Err(Error::State(46));
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
    Ok(prepared.publish())
}

fn create_event(process: &Process) -> Result<HandleValue, Error> {
    let event = Event::try_new(&process.resource_domain()).map_err(|_| Error::Construction)?;
    Ok(process.create_object(event, <Event as KernelObject>::SUPPORTED_RIGHTS)?)
}

fn verify_read_bytes(
    process: &Process,
    receiver: HandleValue,
    output: UserSlice,
    expected: &[u8],
    stage: usize,
) -> Result<(), Error> {
    let outcome = channel_read(
        process,
        receiver,
        ReadBuffers {
            bytes: Some(output),
            handles: None,
        },
    )?;
    if outcome
        != (ChannelReadOutcome::Received {
            bytes: expected.len() as u64,
            handles: 0,
        })
    {
        return Err(Error::State(stage));
    }
    let mut actual = [0; 16];
    let Some(target) = actual.get_mut(..expected.len()) else {
        return Err(Error::State(stage));
    };
    process.copy_from_user(
        UserSlice::new(output.base(), expected.len() as u64).map_err(|_| Error::Construction)?,
        target,
    )?;
    if target != expected {
        return Err(Error::State(stage));
    }
    Ok(())
}

fn verify_empty(process: &Process, receiver: HandleValue, stage: usize) -> Result<(), Error> {
    if matches!(
        channel_read(
            process,
            receiver,
            ReadBuffers {
                bytes: None,
                handles: None,
            },
        ),
        Err(ChannelServiceError::Channel(ChannelError::WouldBlock))
    ) {
        Ok(())
    } else {
        Err(Error::State(stage))
    }
}

fn verify_invalid_handle(process: &Process, value: HandleValue, stage: usize) -> Result<(), Error> {
    if matches!(
        process.handle_info(value, Rights::NONE),
        Err(ProcessError::Handle(HandleError::InvalidHandle))
    ) {
        Ok(())
    } else {
        Err(Error::State(stage))
    }
}

fn write_user(process: &Process, offset: u64, bytes: &[u8]) -> Result<UserSlice, Error> {
    let range = scratch(offset, bytes.len() as u64)?;
    process.copy_to_user(range, bytes)?;
    Ok(range)
}

fn read_handle(process: &Process, range: UserSlice) -> Result<HandleValue, Error> {
    let mut encoded = [0; 8];
    process.copy_from_user(range, &mut encoded)?;
    HandleValue::try_from_raw(u64::from_ne_bytes(encoded))
        .map_err(|error| Error::Process(ProcessError::Handle(error)))
}

fn encode_disposition(
    handle: HandleValue,
    rights: u64,
    expected_kind: u32,
    reserved: u32,
) -> [u8; 24] {
    let mut encoded = [0; 24];
    encoded[0..8].copy_from_slice(&handle.get().to_ne_bytes());
    encoded[8..16].copy_from_slice(&rights.to_ne_bytes());
    encoded[16..20].copy_from_slice(&expected_kind.to_ne_bytes());
    encoded[20..24].copy_from_slice(&reserved.to_ne_bytes());
    encoded
}

fn encode_two_dispositions(first: [u8; 24], second: [u8; 24]) -> [u8; 48] {
    let mut encoded = [0; 48];
    encoded[..24].copy_from_slice(&first);
    encoded[24..].copy_from_slice(&second);
    encoded
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
    // Observation only: production kreaper must own and complete retirement.
    if crate::kernel::task::wait_for_test_progress(
        crate::kernel::task::TEST_PROGRESS_TIMEOUT_NS,
        || Ok::<_, Error>(process.snapshot().phase == ProcessPhase::Retired),
    )? {
        Ok(())
    } else {
        Err(Error::Construction)
    }
}
