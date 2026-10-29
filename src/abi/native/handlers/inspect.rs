// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Native inspect syscall validation.

use crate::abi::native::Arguments;
use crate::capability::InvocationReceiver;
use hyper::abi::native::{
    HYPER_NATIVE_CPU_OBSERVATION_MIN_SIZE, HYPER_NATIVE_MEMORY_OBSERVATION_MIN_SIZE,
    HYPER_NATIVE_OBJECT_DETAILS_MIN_SIZE, HyperNativeCpuObservation, HyperNativeMemoryObservation,
    HyperNativeObjectDetails,
};
use {
    crate::abi::native::services::InspectServices, crate::abi::native::services::NativeAction,
    crate::abi::native::services::SystemInspectServices,
};
use {
    crate::abi::native::status::handle_result, crate::abi::native::status::info_result,
    crate::abi::native::status::scan_result,
    crate::abi::native::status::status_from_inspection_error,
};
use {
    crate::abi::native::wire::copy_encoded_page, crate::abi::native::wire::copy_info_record,
    crate::abi::native::wire::encode_cpu_observation,
    crate::abi::native::wire::encode_handle_inspection,
    crate::abi::native::wire::encode_memory_observation,
    crate::abi::native::wire::encode_object_inspection,
    crate::abi::native::wire::encode_task_process, crate::abi::native::wire::encode_task_thread,
    crate::abi::native::wire::parse_handle, crate::abi::native::wire::parse_handle_inspector_scan,
    crate::abi::native::wire::parse_inspector_scan, crate::abi::native::wire::prepare_info_request,
};
use {crate::diagnostics::inspect::OBJECT_PAGE_CAPACITY, crate::diagnostics::inspect::Page};

#[inline(never)]
pub(in crate::abi::native) fn sys_object_inspector_read_details(
    services: &impl InspectServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = (|| {
        let request = prepare_info_request(
            &[arguments[3], arguments[4], 0, 0, 0, 0],
            HYPER_NATIVE_OBJECT_DETAILS_MIN_SIZE,
            core::mem::size_of::<HyperNativeObjectDetails>(),
        )?;
        let value = services
            .read_object_details(receiver, arguments[0], arguments[1], arguments[2])
            .map_err(status_from_inspection_error)?;
        let record = crate::abi::native::wire::encode_object_details(&value);
        copy_info_record(services, request, &record)
    })();
    NativeAction::Return(info_result(result))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_task_inspector_scan_processes(
    services: &impl InspectServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = parse_inspector_scan(
        arguments,
        crate::diagnostics::inspect::PROCESS_PAGE_CAPACITY,
        hyper::abi::native::HYPER_NATIVE_TASK_PROCESS_MIN_SIZE,
    )
    .and_then(|(cursor, destination)| {
        let mut page = Page::empty();
        services
            .scan_processes(receiver, cursor, &mut page)
            .map_err(status_from_inspection_error)?;
        copy_encoded_page(services, destination, &page, encode_task_process)
    });
    NativeAction::Return(scan_result(result))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_task_inspector_scan_threads(
    services: &impl InspectServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = parse_inspector_scan(
        arguments,
        crate::diagnostics::inspect::THREAD_PAGE_CAPACITY,
        hyper::abi::native::HYPER_NATIVE_TASK_THREAD_MIN_SIZE,
    )
    .and_then(|(cursor, destination)| {
        let mut page = Page::empty();
        services
            .scan_threads(receiver, cursor, &mut page)
            .map_err(status_from_inspection_error)?;
        copy_encoded_page(services, destination, &page, encode_task_thread)
    });
    NativeAction::Return(scan_result(result))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_object_inspector_scan_objects(
    services: &impl InspectServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = parse_inspector_scan(
        arguments,
        OBJECT_PAGE_CAPACITY,
        hyper::abi::native::HYPER_NATIVE_OBJECT_INSPECTION_MIN_SIZE,
    )
    .and_then(|(cursor, destination)| {
        let mut page = Page::empty();
        services
            .scan_objects(receiver, cursor, &mut page)
            .map_err(status_from_inspection_error)?;
        copy_encoded_page(services, destination, &page, encode_object_inspection)
    });
    NativeAction::Return(scan_result(result))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_object_inspector_scan_handles(
    services: &impl InspectServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result =
        parse_handle_inspector_scan(arguments).and_then(|(process_koid, cursor, destination)| {
            let page = services
                .scan_process_handles(receiver, process_koid, cursor)
                .map_err(status_from_inspection_error)?;
            copy_encoded_page(services, destination, &page, encode_handle_inspection)
        });
    NativeAction::Return(scan_result(result))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_task_inspector_derive_process(
    services: &impl InspectServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = parse_handle(arguments[0]).and_then(|process| {
        services
            .derive_task_inspector(receiver, process)
            .map_err(status_from_inspection_error)
    });
    NativeAction::Return(handle_result(result))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_object_inspector_derive_process(
    services: &impl InspectServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = parse_handle(arguments[0]).and_then(|process| {
        services
            .derive_object_inspector(receiver, process)
            .map_err(status_from_inspection_error)
    });
    NativeAction::Return(handle_result(result))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_task_inspector_derive_task_group(
    services: &impl InspectServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = parse_handle(arguments[0]).and_then(|group| {
        services
            .derive_task_inspector_for_task_group(receiver, group)
            .map_err(status_from_inspection_error)
    });
    NativeAction::Return(handle_result(result))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_object_inspector_derive_task_group(
    services: &impl InspectServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = parse_handle(arguments[0]).and_then(|group| {
        services
            .derive_object_inspector_for_task_group(receiver, group)
            .map_err(status_from_inspection_error)
    });
    NativeAction::Return(handle_result(result))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_task_inspector_derive_resource_domain(
    services: &impl InspectServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = parse_handle(arguments[0]).and_then(|domain| {
        services
            .derive_task_inspector_for_resource_domain(receiver, domain)
            .map_err(status_from_inspection_error)
    });
    NativeAction::Return(handle_result(result))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_object_inspector_derive_resource_domain(
    services: &impl InspectServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = parse_handle(arguments[0]).and_then(|domain| {
        services
            .derive_object_inspector_for_resource_domain(receiver, domain)
            .map_err(status_from_inspection_error)
    });
    NativeAction::Return(handle_result(result))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_memory_inspector_read(
    services: &impl SystemInspectServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = prepare_info_request(
        arguments,
        HYPER_NATIVE_MEMORY_OBSERVATION_MIN_SIZE,
        core::mem::size_of::<HyperNativeMemoryObservation>(),
    )
    .and_then(|request| {
        let inspector = receiver;
        let observation = services
            .memory_observation(inspector)
            .map_err(status_from_inspection_error)?;
        copy_info_record(services, request, &encode_memory_observation(observation))
    });
    NativeAction::Return(info_result(result))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_cpu_inspector_read(
    services: &impl SystemInspectServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = prepare_info_request(
        arguments,
        HYPER_NATIVE_CPU_OBSERVATION_MIN_SIZE,
        core::mem::size_of::<HyperNativeCpuObservation>(),
    )
    .and_then(|request| {
        let inspector = receiver;
        let observation = services
            .cpu_observation(inspector)
            .map_err(status_from_inspection_error)?;
        copy_info_record(services, request, &encode_cpu_observation(observation))
    });
    NativeAction::Return(info_result(result))
}
