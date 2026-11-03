// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Guest configuration, execution, and exit records.

use super::{
    copy_extensible_input_record, optional_user_slice, read_record_u32, read_record_u64,
    require_zero, write_u32, write_u64,
};
use crate::abi::native::Arguments;
use crate::abi::native::services::UserMemoryServices;
use crate::mm::user_space::UserSlice;
use hyper::abi::native::{
    HYPER_NATIVE_STATUS_INVALID_ARGUMENT, HYPER_NATIVE_VIRTUAL_CPU_BOOTSTRAP_MIN_SIZE,
    HYPER_NATIVE_VIRTUAL_MACHINE_CONFIGURATION_MIN_SIZE,
    HYPER_NATIVE_VIRTUAL_SERIAL_MAX_TRANSFER_BYTES, HyperNativeStatus,
    HyperNativeVirtualCpuBootstrap, HyperNativeVirtualCpuInfo,
    HyperNativeVirtualMachineConfiguration, HyperNativeVirtualMachineInfo,
};

pub(in crate::abi::native) fn decode_virtual_machine_configuration(
    services: &impl UserMemoryServices,
    arguments: &Arguments,
) -> Result<crate::vm::objects::VirtualMachineConfiguration, HyperNativeStatus> {
    type Record = HyperNativeVirtualMachineConfiguration;
    const SIZE: usize = core::mem::size_of::<Record>();
    require_zero(&arguments[2..])?;
    let record = copy_extensible_input_record::<SIZE>(
        services,
        arguments[0],
        arguments[1],
        HYPER_NATIVE_VIRTUAL_MACHINE_CONFIGURATION_MIN_SIZE,
    )?;
    let flags = read_record_u32(&record, core::mem::offset_of!(Record, flags));
    if flags != 0 {
        return Err(HYPER_NATIVE_STATUS_INVALID_ARGUMENT);
    }
    Ok(crate::vm::objects::VirtualMachineConfiguration {
        guest_physical_base: read_record_u64(
            &record,
            core::mem::offset_of!(Record, guest_physical_base),
        ),
        memory_size: read_record_u64(&record, core::mem::offset_of!(Record, memory_size)),
        vcpu_count: read_record_u32(&record, core::mem::offset_of!(Record, vcpu_count)),
        architecture: read_record_u32(&record, core::mem::offset_of!(Record, architecture)),
        platform_profile: read_record_u32(&record, core::mem::offset_of!(Record, platform_profile)),
    })
}

pub(in crate::abi::native) fn decode_virtual_cpu_bootstrap(
    services: &impl UserMemoryServices,
    arguments: &Arguments,
) -> Result<crate::vm::objects::VirtualCpuBootstrap, HyperNativeStatus> {
    type Record = HyperNativeVirtualCpuBootstrap;
    const SIZE: usize = core::mem::size_of::<Record>();
    require_zero(&arguments[2..])?;
    let record = copy_extensible_input_record::<SIZE>(
        services,
        arguments[0],
        arguments[1],
        HYPER_NATIVE_VIRTUAL_CPU_BOOTSTRAP_MIN_SIZE,
    )?;
    let vcpu_id = read_record_u32(&record, core::mem::offset_of!(Record, vcpu_id));
    let flags = read_record_u32(&record, core::mem::offset_of!(Record, flags));
    let reserved = read_record_u64(&record, core::mem::offset_of!(Record, reserved));
    if vcpu_id != 0 || flags != 0 || reserved != 0 {
        return Err(HYPER_NATIVE_STATUS_INVALID_ARGUMENT);
    }
    Ok(crate::vm::objects::VirtualCpuBootstrap {
        entry: read_record_u64(&record, core::mem::offset_of!(Record, entry)),
        stack: read_record_u64(&record, core::mem::offset_of!(Record, stack)),
        arguments: [
            read_record_u64(&record, core::mem::offset_of!(Record, argument0)),
            read_record_u64(&record, core::mem::offset_of!(Record, argument1)),
            read_record_u64(&record, core::mem::offset_of!(Record, argument2)),
            read_record_u64(&record, core::mem::offset_of!(Record, argument3)),
        ],
    })
}

pub(in crate::abi::native) fn parse_virtual_serial_io(
    arguments: &Arguments,
) -> Result<Option<UserSlice>, HyperNativeStatus> {
    if arguments[1] > HYPER_NATIVE_VIRTUAL_SERIAL_MAX_TRANSFER_BYTES
        || arguments[2..].iter().any(|argument| *argument != 0)
    {
        return Err(HYPER_NATIVE_STATUS_INVALID_ARGUMENT);
    }
    optional_user_slice(arguments[0], arguments[1])
}

pub(in crate::abi::native) fn encode_virtual_machine_info(
    configuration: crate::vm::objects::VirtualMachineConfiguration,
    snapshot: crate::vm::objects::VirtualMachineSnapshot,
) -> [u8; core::mem::size_of::<HyperNativeVirtualMachineInfo>()] {
    type Record = HyperNativeVirtualMachineInfo;
    let mut record = [0_u8; core::mem::size_of::<Record>()];
    write_u64(
        &mut record,
        core::mem::offset_of!(Record, resident_memory_bytes),
        snapshot.resident_memory_bytes.unwrap_or(u64::MAX),
    );
    write_u32(
        &mut record,
        core::mem::offset_of!(Record, phase),
        snapshot.phase,
    );
    write_u32(
        &mut record,
        core::mem::offset_of!(Record, vcpu_count),
        configuration.vcpu_count,
    );
    write_u64(
        &mut record,
        core::mem::offset_of!(Record, guest_physical_base),
        configuration.guest_physical_base,
    );
    write_u64(
        &mut record,
        core::mem::offset_of!(Record, memory_size),
        configuration.memory_size,
    );
    write_u32(
        &mut record,
        core::mem::offset_of!(Record, architecture),
        configuration.architecture,
    );
    write_u32(
        &mut record,
        core::mem::offset_of!(Record, platform_profile),
        configuration.platform_profile,
    );
    record
}

pub(in crate::abi::native) fn encode_virtual_cpu_info(
    snapshot: crate::vm::objects::VirtualCpuSnapshot,
) -> [u8; core::mem::size_of::<HyperNativeVirtualCpuInfo>()] {
    type Record = HyperNativeVirtualCpuInfo;
    let mut record = [0_u8; core::mem::size_of::<Record>()];
    write_u32(
        &mut record,
        core::mem::offset_of!(Record, host_cpu),
        snapshot.host_cpu.unwrap_or(u32::MAX),
    );
    write_u32(
        &mut record,
        core::mem::offset_of!(Record, migration_target),
        snapshot.migration_target.unwrap_or(u32::MAX),
    );
    write_u32(
        &mut record,
        core::mem::offset_of!(Record, vcpu_id),
        snapshot.id,
    );
    write_u32(
        &mut record,
        core::mem::offset_of!(Record, phase),
        snapshot.phase,
    );
    write_u64(
        &mut record,
        core::mem::offset_of!(Record, scheduler_thread_id),
        snapshot.thread,
    );
    write_u32(
        &mut record,
        core::mem::offset_of!(Record, terminal_reason),
        snapshot.terminal_reason,
    );
    record
}

pub(in crate::abi::native) fn encode_virtual_machine_platform_info(
    info: crate::vm::service::VirtualMachinePlatformInfo,
) -> [u8; core::mem::size_of::<hyper::abi::native::HyperNativeVirtualMachinePlatformInfo>()] {
    type Record = hyper::abi::native::HyperNativeVirtualMachinePlatformInfo;
    let mut bytes = [0; core::mem::size_of::<Record>()];
    write_u32(
        &mut bytes,
        core::mem::offset_of!(Record, architecture),
        info.architecture,
    );
    write_u32(
        &mut bytes,
        core::mem::offset_of!(Record, platform_profile),
        info.platform_profile,
    );
    write_u64(
        &mut bytes,
        core::mem::offset_of!(Record, counter_frequency_hz),
        info.counter_frequency_hz,
    );
    write_u64(
        &mut bytes,
        core::mem::offset_of!(Record, riscv_isa),
        info.riscv_isa,
    );
    write_u64(
        &mut bytes,
        core::mem::offset_of!(Record, aarch64_gic_version),
        info.aarch64_gic_version,
    );
    bytes
}

pub(in crate::abi::native) fn encode_virtual_machine_power_request(
    request: hyper_vm_models::arm::psci::Request,
) -> [u8; core::mem::size_of::<hyper::abi::native::HyperNativeVirtualMachinePowerRequest>()] {
    use hyper::abi::native::{
        HYPER_NATIVE_VIRTUAL_MACHINE_POWER_CPU_OFF, HYPER_NATIVE_VIRTUAL_MACHINE_POWER_CPU_ON,
        HYPER_NATIVE_VIRTUAL_MACHINE_POWER_SYSTEM_OFF,
        HYPER_NATIVE_VIRTUAL_MACHINE_POWER_SYSTEM_RESET,
        HyperNativeVirtualMachinePowerRequest as Record,
    };
    use hyper_vm_models::arm::psci::Operation;
    let mut record = [0_u8; core::mem::size_of::<Record>()];
    let operation = match request.operation {
        Operation::CpuOn => HYPER_NATIVE_VIRTUAL_MACHINE_POWER_CPU_ON,
        Operation::CpuOff => HYPER_NATIVE_VIRTUAL_MACHINE_POWER_CPU_OFF,
        Operation::SystemOff => HYPER_NATIVE_VIRTUAL_MACHINE_POWER_SYSTEM_OFF,
        Operation::SystemReset => HYPER_NATIVE_VIRTUAL_MACHINE_POWER_SYSTEM_RESET,
    };
    write_u64(&mut record, core::mem::offset_of!(Record, id), request.id);
    write_u32(
        &mut record,
        core::mem::offset_of!(Record, vcpu),
        request.vcpu,
    );
    write_u32(
        &mut record,
        core::mem::offset_of!(Record, operation),
        operation as u32,
    );
    write_u32(
        &mut record,
        core::mem::offset_of!(Record, target),
        request.target,
    );
    write_u64(
        &mut record,
        core::mem::offset_of!(Record, entry),
        request.entry,
    );
    write_u64(
        &mut record,
        core::mem::offset_of!(Record, context),
        request.context,
    );
    record
}

pub(in crate::abi::native) fn encode_virtual_cpu_mmio_request(
    request: hyper_vm_models::device::mmio::Request,
) -> [u8; core::mem::size_of::<hyper::abi::native::HyperNativeVirtualCpuMmioRequest>()] {
    use hyper::abi::native::HyperNativeVirtualCpuMmioRequest as Record;
    use hyper_vm_models::exit::MmioOperation;
    let mut record = [0_u8; core::mem::size_of::<Record>()];
    let (operation, value) = match request.access.operation() {
        MmioOperation::Read => (0, 0),
        MmioOperation::Write(value) => (1, value),
    };
    write_u64(&mut record, core::mem::offset_of!(Record, id), request.id);
    write_u64(
        &mut record,
        core::mem::offset_of!(Record, device),
        request.device,
    );
    write_u64(
        &mut record,
        core::mem::offset_of!(Record, address),
        request.access.address().get(),
    );
    write_u64(&mut record, core::mem::offset_of!(Record, value), value);
    write_u32(
        &mut record,
        core::mem::offset_of!(Record, operation),
        operation,
    );
    write_u32(
        &mut record,
        core::mem::offset_of!(Record, width),
        request.access.size() as u32,
    );
    record
}
