// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Bounded postmortem operations. Each output is copied from an owned record;
//! no target lock is held while touching the observer's userspace memory.

use crate::capability::handle_wire;

use crate::capability::InvocationReceiver;

use crate::abi::native::services::NativeAction;
use crate::process::TerminalReason;
use hyper::abi::native as abi;
use {crate::abi::native::Arguments, crate::abi::native::RetentionServices};
use {
    crate::abi::native::status::failure, crate::abi::native::status::handle_result,
    crate::abi::native::status::status_from_process_builder_service_error,
    crate::abi::native::status::status_from_process_error,
    crate::abi::native::status::status_from_retention_error,
    crate::abi::native::status::status_only, crate::abi::native::status::success,
};
use {
    crate::abi::native::wire::require_zero, crate::abi::native::wire::terminal_reason,
    crate::abi::native::wire::write_u32, crate::abi::native::wire::write_u64,
};
use {
    crate::mm::user_space::Access, crate::mm::user_space::CrashMappingKind,
    crate::mm::user_space::UserAddress, crate::mm::user_space::UserSlice,
};

#[inline(never)]
pub(in crate::abi::native) fn sys_process_builder_retain(
    services: &impl RetentionServices,
    receiver: &InvocationReceiver,
    args: &Arguments,
) -> NativeAction {
    let result = (|| {
        require_zero(&args[2..])?;
        services
            .retain_process_builder(receiver, args[0], args[1])
            .map_err(status_from_process_builder_service_error)
    })();
    NativeAction::Return(handle_result(result))
}
#[inline(never)]
pub(in crate::abi::native) fn sys_process_retirement_open_view(
    services: &impl RetentionServices,
    receiver: &InvocationReceiver,
    args: &Arguments,
) -> NativeAction {
    let result = (|| {
        require_zero(&args[0..])?;
        services
            .retention_open_view(receiver)
            .map_err(status_from_retention_error)
    })();
    NativeAction::Return(handle_result(result))
}
#[inline(never)]
pub(in crate::abi::native) fn sys_process_retirement_release(
    services: &impl RetentionServices,
    receiver: &InvocationReceiver,
    args: &Arguments,
) -> NativeAction {
    let result = (|| {
        require_zero(&args[0..])?;
        services
            .retention_release(receiver)
            .map_err(status_from_retention_error)
    })();
    NativeAction::Return(status_only(result))
}
#[inline(never)]
pub(in crate::abi::native) fn sys_retention_info(
    services: &impl RetentionServices,
    receiver: &InvocationReceiver,
    args: &Arguments,
    view: bool,
) -> NativeAction {
    let result = (|| {
        require_zero(&args[2..])?;
        let output = output(
            args[0],
            args[1],
            abi::HYPER_NATIVE_PROCESS_RETIREMENT_INFO_MIN_SIZE,
        )?;
        let info = services
            .retention_info(receiver, view)
            .map_err(status_from_retention_error)?;
        let mut record = [0; abi::HYPER_NATIVE_PROCESS_RETIREMENT_INFO_MIN_SIZE];
        write_u32(
            &mut record,
            0,
            abi::HYPER_NATIVE_PROCESS_RETIREMENT_INFO_MIN_SIZE as u32,
        );
        write_u32(&mut record, 4, info.phase);
        write_u32(&mut record, 8, info.flags);
        write_u32(&mut record, 12, info.release_reason);
        write_u64(&mut record, 16, info.process_koid);
        write_u64(&mut record, 24, info.deadline_ns);
        write_u64(&mut record, 32, info.retained_bytes);
        write_u64(&mut record, 40, info.memory_limit);
        write_u32(&mut record, 48, terminal_reason(info.terminal));
        let (class, status) = match info.terminal {
            Some(TerminalReason::Fault { class, .. }) => (class, 0),
            Some(
                TerminalReason::ThreadExited { status }
                | TerminalReason::ProcessExited { status }
                | TerminalReason::LastThreadExited { status },
            ) => (0, status),
            _ => (0, 0),
        };
        write_u32(&mut record, 52, class);
        write_u64(&mut record, 56, status as u64);
        write_u64(&mut record, 64, info.fault_pc);
        write_u64(&mut record, 72, info.fault_address);
        write_u64(&mut record, 80, info.fault_syndrome);
        write_u32(&mut record, 88, info.thread_count as u32);
        write_u32(&mut record, 92, info.mapping_count as u32);
        write_u32(&mut record, 96, info.handle_count as u32);
        services
            .copy_to_user(output, &record)
            .map_err(status_from_process_error)?;
        Ok([record.len() as u64, 0])
    })();
    NativeAction::Return(result.map_or_else(failure, success))
}
#[inline(never)]
pub(in crate::abi::native) fn sys_crash_view_thread(
    services: &impl RetentionServices,
    receiver: &InvocationReceiver,
    args: &Arguments,
) -> NativeAction {
    let result = (|| {
        require_zero(&args[3..])?;
        let output = output(args[1], args[2], abi::HYPER_NATIVE_CRASH_THREAD_MIN_SIZE)?;
        let (thread, next) = services
            .crash_thread(receiver, index(args[0])?)
            .map_err(status_from_retention_error)?;
        let mut record = [0; abi::HYPER_NATIVE_CRASH_THREAD_MIN_SIZE];
        write_u32(
            &mut record,
            0,
            abi::HYPER_NATIVE_CRASH_THREAD_MIN_SIZE as u32,
        );
        write_u32(&mut record, 4, thread.registers.machine);
        write_u64(&mut record, 16, thread.koid);
        for (index, value) in [
            thread.registers.pc,
            thread.registers.sp,
            thread.registers.tls,
            thread.registers.processor_state,
        ]
        .iter()
        .enumerate()
        {
            write_u64(&mut record, 24 + index * 8, *value);
        }
        for (index, value) in thread
            .registers
            .general
            .iter()
            .chain(thread.registers.vector.iter())
            .chain(thread.registers.fp_control.iter())
            .enumerate()
        {
            write_u64(&mut record, 56 + index * 8, *value);
        }
        services
            .copy_to_user(output, &record)
            .map_err(status_from_process_error)?;
        Ok([record.len() as u64, next as u64])
    })();
    NativeAction::Return(result.map_or_else(failure, success))
}
#[inline(never)]
pub(in crate::abi::native) fn sys_crash_view_mapping(
    services: &impl RetentionServices,
    receiver: &InvocationReceiver,
    args: &Arguments,
) -> NativeAction {
    let result = (|| {
        require_zero(&args[3..])?;
        let output = output(args[1], args[2], abi::HYPER_NATIVE_CRASH_MAPPING_MIN_SIZE)?;
        let (mapping, next) = services
            .crash_mapping(receiver, index(args[0])?)
            .map_err(status_from_retention_error)?;
        let mut record = [0; abi::HYPER_NATIVE_CRASH_MAPPING_MIN_SIZE];
        write_u32(
            &mut record,
            0,
            abi::HYPER_NATIVE_CRASH_MAPPING_MIN_SIZE as u32,
        );
        write_u32(&mut record, 4, mapping.flags);
        write_u64(&mut record, 8, mapping.base);
        write_u64(&mut record, 16, mapping.length);
        write_u64(&mut record, 24, mapping.object_offset);
        let permissions = u32::from(mapping.permissions.contains(Access::Read))
            | u32::from(mapping.permissions.contains(Access::Write)) << 1
            | u32::from(mapping.permissions.contains(Access::Execute)) << 2;
        write_u32(&mut record, 40, permissions);
        write_u32(
            &mut record,
            44,
            match mapping.kind {
                CrashMappingKind::Anonymous => 1,
                CrashMappingKind::PrivateFile => 2,
                CrashMappingKind::Shared => 3,
            },
        );
        services
            .copy_to_user(output, &record)
            .map_err(status_from_process_error)?;
        Ok([record.len() as u64, next as u64])
    })();
    NativeAction::Return(result.map_or_else(failure, success))
}
#[inline(never)]
pub(in crate::abi::native) fn sys_crash_view_handle(
    services: &impl RetentionServices,
    receiver: &InvocationReceiver,
    args: &Arguments,
) -> NativeAction {
    let result = (|| {
        require_zero(&args[3..])?;
        let output = output(args[1], args[2], abi::HYPER_NATIVE_CRASH_HANDLE_MIN_SIZE)?;
        let (handle, next) = services
            .crash_handle(receiver, index(args[0])?)
            .map_err(status_from_retention_error)?;
        let mut record = [0; abi::HYPER_NATIVE_CRASH_HANDLE_MIN_SIZE];
        write_u32(
            &mut record,
            0,
            abi::HYPER_NATIVE_CRASH_HANDLE_MIN_SIZE as u32,
        );
        write_u32(&mut record, 4, handle.info.kind.get());
        write_u64(&mut record, 8, handle_wire::to_abi(handle.value));
        write_u64(&mut record, 16, handle.info.koid.get());
        write_u64(&mut record, 24, handle.info.rights.bits());
        write_u32(&mut record, 32, handle.info.flags.bits());
        services
            .copy_to_user(output, &record)
            .map_err(status_from_process_error)?;
        Ok([record.len() as u64, next as u64])
    })();
    NativeAction::Return(result.map_or_else(failure, success))
}
#[inline(never)]
pub(in crate::abi::native) fn sys_crash_view_read(
    services: &impl RetentionServices,
    receiver: &InvocationReceiver,
    args: &Arguments,
) -> NativeAction {
    let result = (|| {
        require_zero(&args[3..])?;
        if args[2] == 0 || args[2] > abi::HYPER_NATIVE_CRASH_READ_MAX_BYTES {
            return Err(abi::HYPER_NATIVE_STATUS_INVALID_ARGUMENT);
        }
        let output = UserSlice::new(UserAddress::new(args[1]), args[2])
            .map_err(|_| abi::HYPER_NATIVE_STATUS_INVALID_ARGUMENT)?;
        services
            .crash_read(receiver, args[0], output)
            .map(|count| [count as u64, 0])
            .map_err(status_from_retention_error)
    })();
    NativeAction::Return(result.map_or_else(failure, success))
}
fn output(address: u64, size: u64, required: usize) -> Result<UserSlice, abi::HyperNativeStatus> {
    if size < required as u64 {
        return Err(abi::HYPER_NATIVE_STATUS_BUFFER_TOO_SMALL);
    }
    if size > abi::HYPER_NATIVE_EXTENSIBLE_RECORD_MAX_BYTES {
        return Err(abi::HYPER_NATIVE_STATUS_INVALID_ARGUMENT);
    }
    UserSlice::new(UserAddress::new(address), required as u64)
        .map_err(|_| abi::HYPER_NATIVE_STATUS_INVALID_ARGUMENT)
}
fn index(raw: u64) -> Result<usize, abi::HyperNativeStatus> {
    usize::try_from(raw).map_err(|_| abi::HYPER_NATIVE_STATUS_INVALID_ARGUMENT)
}
