// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Process construction, affinity, and lifecycle records.

use super::object::parse_object_kind;
use super::{optional_user_slice, parse_handle, read_record_u64, write_u32, write_u64};
use crate::abi::native::Arguments;
use crate::capability::{HandleValue, Rights};
use crate::mm::user_space::UserSlice;
use crate::process::{ProcessPhase, ProcessSnapshot, TerminalReason};
use hyper::abi::native::{
    HYPER_NATIVE_CAPABILITY_DISPOSITION_DUPLICATE, HYPER_NATIVE_CAPABILITY_DISPOSITION_MOVE,
    HYPER_NATIVE_CAPABILITY_DISPOSITION_SAME_RIGHTS, HYPER_NATIVE_PROCESS_AFFINITY_MAX_WORDS,
    HYPER_NATIVE_PROCESS_PHASE_CREATED, HYPER_NATIVE_PROCESS_PHASE_PREPARED,
    HYPER_NATIVE_PROCESS_PHASE_RETIRED, HYPER_NATIVE_PROCESS_PHASE_RETIRING,
    HYPER_NATIVE_PROCESS_PHASE_RUNNING, HYPER_NATIVE_PROCESS_PHASE_STOPPED,
    HYPER_NATIVE_PROCESS_PHASE_STOPPING, HYPER_NATIVE_PROCESS_TERMINAL_FAULT,
    HYPER_NATIVE_PROCESS_TERMINAL_LAST_THREAD_EXITED, HYPER_NATIVE_PROCESS_TERMINAL_NONE,
    HYPER_NATIVE_PROCESS_TERMINAL_PROCESS_EXITED, HYPER_NATIVE_PROCESS_TERMINAL_REQUESTED,
    HYPER_NATIVE_PROCESS_TERMINAL_TASK_GROUP_STOP, HYPER_NATIVE_PROCESS_TERMINAL_THREAD_EXITED,
    HYPER_NATIVE_STATUS_INVALID_ARGUMENT, HyperNativeProcessInfo, HyperNativeStatus,
};

pub(in crate::abi::native) const PROCESS_INFO_SIZE: usize =
    core::mem::size_of::<HyperNativeProcessInfo>();

pub(in crate::abi::native) struct ProcessBuilderHandleRequest {
    pub source: HandleValue,
    pub purpose: u32,
    pub expected_kind: crate::object::ObjectKind,
    pub requested_rights: Option<Rights>,
    pub operation: crate::capability::HandleTransferOperation,
}

pub(in crate::abi::native) fn parse_builder_create(
    arguments: &Arguments,
) -> Result<(HandleValue, HandleValue, HandleValue, HandleValue), HyperNativeStatus> {
    Ok((
        parse_handle(arguments[0])?,
        parse_handle(arguments[1])?,
        parse_handle(arguments[2])?,
        parse_handle(arguments[3])?,
    ))
}

pub(in crate::abi::native) fn parse_builder_text(
    arguments: &Arguments,
    maximum_bytes: u64,
) -> Result<Option<UserSlice>, HyperNativeStatus> {
    if arguments[1] > maximum_bytes {
        return Err(HYPER_NATIVE_STATUS_INVALID_ARGUMENT);
    }
    optional_user_slice(arguments[0], arguments[1])
}

pub(in crate::abi::native) fn parse_affinity_words(
    pointer: u64,
    count: u64,
) -> Result<(Option<UserSlice>, usize), HyperNativeStatus> {
    if pointer == 0 || count == 0 || count > HYPER_NATIVE_PROCESS_AFFINITY_MAX_WORDS {
        return Err(HYPER_NATIVE_STATUS_INVALID_ARGUMENT);
    }
    let bytes = count
        .checked_mul(core::mem::size_of::<u64>() as u64)
        .ok_or(HYPER_NATIVE_STATUS_INVALID_ARGUMENT)?;
    let word_count = usize::try_from(count).map_err(|_| HYPER_NATIVE_STATUS_INVALID_ARGUMENT)?;
    Ok((optional_user_slice(pointer, bytes)?, word_count))
}

/// Validated creation parameters; borrowed affinity is bounded separately.
pub(in crate::abi::native) struct ThreadCreateRequest {
    pub(in crate::abi::native) registers: [u64; 4],
    pub(in crate::abi::native) affinity: Option<UserSlice>,
    pub(in crate::abi::native) word_count: usize,
}

pub(in crate::abi::native) fn decode_thread_create(
    request: &[u8; 64],
) -> Result<ThreadCreateRequest, HyperNativeStatus> {
    if read_record_u64(request, 48) != 0 || read_record_u64(request, 56) != 0 {
        return Err(HYPER_NATIVE_STATUS_INVALID_ARGUMENT);
    }
    let pointer = read_record_u64(request, 32);
    let count = read_record_u64(request, 40);
    let (affinity, word_count) = if pointer == 0 && count == 0 {
        (None, 0)
    } else {
        parse_affinity_words(pointer, count)?
    };
    Ok(ThreadCreateRequest {
        registers: core::array::from_fn(|i| read_record_u64(request, i * 8)),
        affinity,
        word_count,
    })
}

pub(in crate::abi::native) fn parse_affinity_request(
    arguments: &Arguments,
) -> Result<(Option<UserSlice>, usize), HyperNativeStatus> {
    if arguments[2..].iter().any(|value| *value != 0) {
        return Err(HYPER_NATIVE_STATUS_INVALID_ARGUMENT);
    }
    parse_affinity_words(arguments[0], arguments[1])
}

pub(in crate::abi::native) fn parse_builder_handle(
    arguments: &Arguments,
) -> Result<ProcessBuilderHandleRequest, HyperNativeStatus> {
    let purpose = u32::try_from(arguments[1]).map_err(|_| HYPER_NATIVE_STATUS_INVALID_ARGUMENT)?;
    let raw_kind = u32::try_from(arguments[2]).map_err(|_| HYPER_NATIVE_STATUS_INVALID_ARGUMENT)?;
    let expected_kind = parse_object_kind(raw_kind)?;
    let requested_rights = if arguments[3] == HYPER_NATIVE_CAPABILITY_DISPOSITION_SAME_RIGHTS {
        None
    } else {
        Some(Rights::from_bits(arguments[3]).ok_or(HYPER_NATIVE_STATUS_INVALID_ARGUMENT)?)
    };
    let operation = match arguments[4] {
        HYPER_NATIVE_CAPABILITY_DISPOSITION_MOVE => {
            crate::capability::HandleTransferOperation::Move
        }
        HYPER_NATIVE_CAPABILITY_DISPOSITION_DUPLICATE => {
            crate::capability::HandleTransferOperation::Copy
        }
        _ => return Err(HYPER_NATIVE_STATUS_INVALID_ARGUMENT),
    };
    Ok(ProcessBuilderHandleRequest {
        source: parse_handle(arguments[0])?,
        purpose,
        expected_kind,
        requested_rights,
        operation,
    })
}

pub(in crate::abi::native) fn encode_process_info(
    snapshot: ProcessSnapshot,
) -> [u8; PROCESS_INFO_SIZE] {
    let (reason, detail0, detail1) = match snapshot.terminal {
        None => (HYPER_NATIVE_PROCESS_TERMINAL_NONE, 0, 0),
        Some(TerminalReason::Requested) => (HYPER_NATIVE_PROCESS_TERMINAL_REQUESTED, 0, 0),
        Some(TerminalReason::ThreadExited { status }) => (
            HYPER_NATIVE_PROCESS_TERMINAL_THREAD_EXITED,
            status as u64,
            0,
        ),
        Some(TerminalReason::ProcessExited { status }) => (
            HYPER_NATIVE_PROCESS_TERMINAL_PROCESS_EXITED,
            status as u64,
            0,
        ),
        Some(TerminalReason::LastThreadExited { status }) => (
            HYPER_NATIVE_PROCESS_TERMINAL_LAST_THREAD_EXITED,
            status as u64,
            0,
        ),
        Some(TerminalReason::Fault { class, code }) => {
            (HYPER_NATIVE_PROCESS_TERMINAL_FAULT, u64::from(class), code)
        }
        Some(TerminalReason::TaskGroupStop { generation }) => {
            (HYPER_NATIVE_PROCESS_TERMINAL_TASK_GROUP_STOP, generation, 0)
        }
    };
    encode_process_info_fields(
        process_phase(snapshot.phase),
        reason as u32,
        detail0,
        detail1,
    )
}

pub(in crate::abi::native) fn encode_process_info_fields(
    phase: u32,
    reason: u32,
    detail0: u64,
    detail1: u64,
) -> [u8; PROCESS_INFO_SIZE] {
    let mut record = [0_u8; PROCESS_INFO_SIZE];
    write_u32(
        &mut record,
        core::mem::offset_of!(HyperNativeProcessInfo, phase),
        phase,
    );
    write_u32(
        &mut record,
        core::mem::offset_of!(HyperNativeProcessInfo, terminal_reason),
        reason,
    );
    write_u64(
        &mut record,
        core::mem::offset_of!(HyperNativeProcessInfo, detail0),
        detail0,
    );
    write_u64(
        &mut record,
        core::mem::offset_of!(HyperNativeProcessInfo, detail1),
        detail1,
    );
    record
}

pub(in crate::abi::native) fn terminal_reason(reason: Option<TerminalReason>) -> u32 {
    match reason {
        None => HYPER_NATIVE_PROCESS_TERMINAL_NONE as u32,
        Some(TerminalReason::Requested) => HYPER_NATIVE_PROCESS_TERMINAL_REQUESTED as u32,
        Some(TerminalReason::ThreadExited { .. }) => {
            HYPER_NATIVE_PROCESS_TERMINAL_THREAD_EXITED as u32
        }
        Some(TerminalReason::ProcessExited { .. }) => {
            HYPER_NATIVE_PROCESS_TERMINAL_PROCESS_EXITED as u32
        }
        Some(TerminalReason::LastThreadExited { .. }) => {
            HYPER_NATIVE_PROCESS_TERMINAL_LAST_THREAD_EXITED as u32
        }
        Some(TerminalReason::Fault { .. }) => HYPER_NATIVE_PROCESS_TERMINAL_FAULT as u32,
        Some(TerminalReason::TaskGroupStop { .. }) => {
            HYPER_NATIVE_PROCESS_TERMINAL_TASK_GROUP_STOP as u32
        }
    }
}

pub(in crate::abi::native) const fn process_phase(phase: ProcessPhase) -> u32 {
    match phase {
        ProcessPhase::Prepared => HYPER_NATIVE_PROCESS_PHASE_PREPARED as u32,
        ProcessPhase::Created => HYPER_NATIVE_PROCESS_PHASE_CREATED as u32,
        ProcessPhase::Running => HYPER_NATIVE_PROCESS_PHASE_RUNNING as u32,
        ProcessPhase::Stopping => HYPER_NATIVE_PROCESS_PHASE_STOPPING as u32,
        ProcessPhase::Stopped => HYPER_NATIVE_PROCESS_PHASE_STOPPED as u32,
        ProcessPhase::Retiring => HYPER_NATIVE_PROCESS_PHASE_RETIRING as u32,
        ProcessPhase::Retired => HYPER_NATIVE_PROCESS_PHASE_RETIRED as u32,
    }
}
