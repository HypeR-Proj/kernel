// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Native vm syscall validation.

use crate::capability::handle_wire;

use crate::abi::native::Arguments;
use crate::capability::InvocationReceiver;
use hyper::abi::native::{
    HYPER_NATIVE_STATUS_INVALID_ARGUMENT, HYPER_NATIVE_VIRTUAL_CPU_INFO_MIN_SIZE,
    HYPER_NATIVE_VIRTUAL_MACHINE_INFO_MIN_SIZE, HyperNativeVirtualCpuInfo,
    HyperNativeVirtualMachineInfo,
};
use {crate::abi::native::services::NativeAction, crate::abi::native::services::VmServices};
use {
    crate::abi::native::status::console_io_result, crate::abi::native::status::failure,
    crate::abi::native::status::handle_result, crate::abi::native::status::info_result,
    crate::abi::native::status::status_from_vm_service_error,
    crate::abi::native::status::status_only, crate::abi::native::status::success,
};
use {
    crate::abi::native::wire::copy_info_record,
    crate::abi::native::wire::decode_virtual_cpu_bootstrap,
    crate::abi::native::wire::decode_virtual_machine_configuration,
    crate::abi::native::wire::encode_virtual_cpu_info,
    crate::abi::native::wire::encode_virtual_machine_info,
    crate::abi::native::wire::parse_affinity_request, crate::abi::native::wire::parse_handle,
    crate::abi::native::wire::parse_virtual_serial_io,
    crate::abi::native::wire::prepare_info_request, crate::abi::native::wire::require_zero,
};

#[inline(never)]
pub(in crate::abi::native) fn sys_virtual_machine_creation_lease_create(
    services: &impl VmServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = require_zero(&arguments[1..])
        .and_then(|()| parse_handle(arguments[0]))
        .and_then(|domain| {
            services
                .derive_virtual_machine_creation_lease(receiver, domain)
                .map_err(status_from_vm_service_error)
        });
    NativeAction::Return(handle_result(result))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_virtual_machine_create(
    services: &impl VmServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = (|| {
        let lease = receiver;
        let configuration = decode_virtual_machine_configuration(services, arguments)?;
        services
            .create_pending_virtual_machine(lease, configuration)
            .map_err(status_from_vm_service_error)
    })();
    NativeAction::Return(handle_result(result))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_pending_virtual_machine_set_memory(
    services: &impl VmServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = require_zero(&arguments[1..])
        .and_then(|()| parse_handle(arguments[0]))
        .and_then(|vmo| {
            services
                .set_pending_virtual_machine_memory(receiver, vmo)
                .map_err(status_from_vm_service_error)
        });
    NativeAction::Return(status_only(result))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_pending_virtual_machine_set_bootstrap(
    services: &impl VmServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = (|| {
        let pending = receiver;
        let bootstrap = decode_virtual_cpu_bootstrap(services, arguments)?;
        services
            .set_pending_virtual_machine_bootstrap(pending, bootstrap)
            .map_err(status_from_vm_service_error)
    })();
    NativeAction::Return(status_only(result))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_pending_virtual_machine_set_virtual_serial(
    services: &impl VmServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = require_zero(&arguments[1..])
        .and_then(|()| parse_handle(arguments[0]))
        .and_then(|serial| {
            services
                .set_pending_virtual_machine_virtual_serial(receiver, serial)
                .map_err(status_from_vm_service_error)
        });
    NativeAction::Return(status_only(result))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_virtual_serial_create(
    services: &impl VmServices,
    _arguments: &Arguments,
) -> NativeAction {
    NativeAction::Return(handle_result(
        services
            .create_virtual_serial()
            .map_err(status_from_vm_service_error),
    ))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_virtual_serial_register_output(
    services: &impl VmServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    NativeAction::Return(status_only(
        require_zero(&arguments[1..])
            .and_then(|()| parse_handle(arguments[0]))
            .and_then(|buffer| {
                services
                    .register_virtual_serial_output(receiver, buffer)
                    .map_err(status_from_vm_service_error)
            }),
    ))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_virtual_serial_acknowledge_output(
    services: &impl VmServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = require_zero(&arguments[1..]).and_then(|()| {
        let serial = receiver;
        services
            .acknowledge_virtual_serial_output(serial, arguments[0])
            .map_err(status_from_vm_service_error)
    });
    NativeAction::Return(status_only(result))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_virtual_serial_write(
    services: &impl VmServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = parse_virtual_serial_io(arguments).and_then(|bytes| {
        services
            .write_virtual_serial(receiver, bytes)
            .map_err(status_from_vm_service_error)
    });
    NativeAction::Return(console_io_result(result))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_pending_virtual_machine_seal(
    services: &impl VmServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    NativeAction::Return(status_only(require_zero(arguments).and_then(|()| {
        services
            .seal_pending_virtual_machine(receiver)
            .map_err(status_from_vm_service_error)
    })))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_pending_virtual_machine_install(
    services: &impl VmServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = require_zero(arguments).and_then(|()| {
        services
            .install_pending_virtual_machine(receiver)
            .map_err(status_from_vm_service_error)
    });
    NativeAction::Return(match result {
        Ok([machine, vcpu]) => success([handle_wire::to_abi(machine), handle_wire::to_abi(vcpu)]),
        Err(status) => failure(status),
    })
}

#[inline(never)]
pub(in crate::abi::native) fn sys_virtual_cpu_set_affinity(
    services: &impl VmServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = parse_affinity_request(arguments).and_then(|(words, count)| {
        services
            .set_virtual_cpu_affinity(receiver, words, count)
            .map_err(status_from_vm_service_error)
    });
    NativeAction::Return(status_only(result))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_virtual_cpu_start(
    services: &impl VmServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    NativeAction::Return(status_only(require_zero(arguments).and_then(|()| {
        services
            .start_virtual_cpu(receiver)
            .map_err(status_from_vm_service_error)
    })))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_pending_virtual_machine_abort(
    services: &impl VmServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    NativeAction::Return(status_only(require_zero(arguments).and_then(|()| {
        services
            .abort_pending_virtual_machine(receiver)
            .map_err(status_from_vm_service_error)
    })))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_virtual_machine_request_stop(
    services: &impl VmServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    NativeAction::Return(status_only(require_zero(arguments).and_then(|()| {
        services
            .request_virtual_machine_stop(receiver)
            .map_err(status_from_vm_service_error)
    })))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_virtual_machine_get_info(
    services: &impl VmServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = prepare_info_request(
        arguments,
        HYPER_NATIVE_VIRTUAL_MACHINE_INFO_MIN_SIZE,
        core::mem::size_of::<HyperNativeVirtualMachineInfo>(),
    )
    .and_then(|request| {
        let machine = receiver;
        let (configuration, snapshot) = services
            .virtual_machine_info(machine)
            .map_err(status_from_vm_service_error)?;
        copy_info_record(
            services,
            request,
            &encode_virtual_machine_info(configuration, snapshot),
        )
    });
    NativeAction::Return(info_result(result))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_virtual_cpu_get_info(
    services: &impl VmServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = prepare_info_request(
        arguments,
        HYPER_NATIVE_VIRTUAL_CPU_INFO_MIN_SIZE,
        core::mem::size_of::<HyperNativeVirtualCpuInfo>(),
    )
    .and_then(|request| {
        let vcpu = receiver;
        let snapshot = services
            .virtual_cpu_info(vcpu)
            .map_err(status_from_vm_service_error)?;
        copy_info_record(services, request, &encode_virtual_cpu_info(snapshot))
    });
    NativeAction::Return(info_result(result))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_virtual_machine_creation_lease_get_platform_info(
    services: &impl VmServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    use hyper::abi::native::{
        HYPER_NATIVE_VIRTUAL_MACHINE_PLATFORM_INFO_MIN_SIZE, HyperNativeVirtualMachinePlatformInfo,
    };
    let result = (|| {
        require_zero(&arguments[3..])?;
        let profile =
            u32::try_from(arguments[0]).map_err(|_| HYPER_NATIVE_STATUS_INVALID_ARGUMENT)?;
        let request = prepare_info_request(
            &[arguments[1], arguments[2], 0, 0, 0, 0],
            HYPER_NATIVE_VIRTUAL_MACHINE_PLATFORM_INFO_MIN_SIZE,
            core::mem::size_of::<HyperNativeVirtualMachinePlatformInfo>(),
        )?;
        let info = services
            .virtual_machine_platform_info(receiver, profile)
            .map_err(status_from_vm_service_error)?;
        copy_info_record(
            services,
            request,
            &crate::abi::native::wire::encode_virtual_machine_platform_info(info),
        )
    })();
    NativeAction::Return(info_result(result))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_virtual_machine_get_power_request(
    services: &impl VmServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    use hyper::abi::native::{
        HYPER_NATIVE_STATUS_WOULD_BLOCK, HYPER_NATIVE_VIRTUAL_MACHINE_POWER_REQUEST_MIN_SIZE,
        HyperNativeVirtualMachinePowerRequest,
    };
    let result = prepare_info_request(
        arguments,
        HYPER_NATIVE_VIRTUAL_MACHINE_POWER_REQUEST_MIN_SIZE,
        core::mem::size_of::<HyperNativeVirtualMachinePowerRequest>(),
    )
    .and_then(|output| {
        // Inspection is non-consuming. A failed copyout leaves this exact
        // request pending; only explicit ID completion advances ownership.
        let request = services
            .pending_power_request(receiver)
            .map_err(status_from_vm_service_error)?
            .ok_or(HYPER_NATIVE_STATUS_WOULD_BLOCK)?;
        copy_info_record(
            services,
            output,
            &crate::abi::native::wire::encode_virtual_machine_power_request(request),
        )
    });
    NativeAction::Return(info_result(result))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_virtual_machine_complete_power_request(
    services: &impl VmServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = (|| {
        require_zero(&arguments[2..])?;
        let machine = receiver;
        if arguments[0] == 0 || arguments[1] > 1 {
            return Err(HYPER_NATIVE_STATUS_INVALID_ARGUMENT);
        }
        services
            .complete_power_request(machine, arguments[0], arguments[1] == 1)
            .map_err(status_from_vm_service_error)
    })();
    NativeAction::Return(status_only(result))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_virtual_machine_open_vcpu(
    services: &impl VmServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = (|| {
        require_zero(&arguments[1..])?;
        let machine = receiver;
        let vcpu = u32::try_from(arguments[0]).map_err(|_| HYPER_NATIVE_STATUS_INVALID_ARGUMENT)?;
        services
            .open_vcpu(machine, vcpu)
            .map_err(status_from_vm_service_error)
    })();
    NativeAction::Return(handle_result(result))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_virtual_machine_register_mmio(
    services: &impl VmServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = (|| {
        require_zero(&arguments[3..])?;
        let machine = receiver;
        if arguments[1] == 0
            || arguments[2] == 0
            || arguments[0].checked_add(arguments[1]).is_none()
        {
            return Err(HYPER_NATIVE_STATUS_INVALID_ARGUMENT);
        }
        services
            .register_mmio(machine, arguments[0], arguments[1], arguments[2])
            .map_err(status_from_vm_service_error)
    })();
    NativeAction::Return(status_only(result))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_virtual_cpu_get_mmio_request(
    services: &impl VmServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    use hyper::abi::native::{
        HYPER_NATIVE_STATUS_WOULD_BLOCK, HYPER_NATIVE_VIRTUAL_CPU_MMIO_REQUEST_MIN_SIZE,
        HyperNativeVirtualCpuMmioRequest,
    };
    let result = prepare_info_request(
        arguments,
        HYPER_NATIVE_VIRTUAL_CPU_MMIO_REQUEST_MIN_SIZE,
        core::mem::size_of::<HyperNativeVirtualCpuMmioRequest>(),
    )
    .and_then(|output| {
        let request = services
            .pending_mmio(receiver)
            .map_err(status_from_vm_service_error)?
            .ok_or(HYPER_NATIVE_STATUS_WOULD_BLOCK)?;
        copy_info_record(
            services,
            output,
            &crate::abi::native::wire::encode_virtual_cpu_mmio_request(request),
        )
    });
    NativeAction::Return(info_result(result))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_virtual_cpu_complete_mmio(
    services: &impl VmServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    use hyper_vm_models::exit::MmioAction;
    let result = (|| {
        require_zero(&arguments[3..])?;
        let vcpu = receiver;
        if arguments[0] == 0 {
            return Err(HYPER_NATIVE_STATUS_INVALID_ARGUMENT);
        }
        let action = match (arguments[1], arguments[2]) {
            (0, value) => MmioAction::CompleteRead(value),
            (1, 0) => MmioAction::CompleteWrite,
            (2, 0) => MmioAction::Stop,
            _ => return Err(HYPER_NATIVE_STATUS_INVALID_ARGUMENT),
        };
        services
            .complete_mmio(vcpu, arguments[0], action)
            .map_err(status_from_vm_service_error)
    })();
    NativeAction::Return(status_only(result))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_guest_memory_create(
    services: &impl VmServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = require_zero(arguments).and_then(|()| {
        services
            .create_guest_memory(receiver)
            .map_err(status_from_vm_service_error)
    });
    NativeAction::Return(handle_result(result))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_pending_virtual_machine_map_memory(
    services: &impl VmServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = (|| {
        require_zero(&arguments[4..])?;
        let pending = receiver;
        let memory = parse_handle(arguments[0])?;
        services
            .map_guest_memory(pending, memory, arguments[1], arguments[2], arguments[3])
            .map_err(status_from_vm_service_error)
    })();
    NativeAction::Return(status_only(result))
}
