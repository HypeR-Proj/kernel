// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! System and object observation records.

use super::process::{process_phase, terminal_reason};
use super::{usize_to_u64, validate_record_stride, write_u32, write_u64};
use crate::abi::native::Arguments;
use crate::abi::native::status::status_from_address_error;
use crate::capability::handle_wire;
use crate::diagnostics::inspect::{
    HANDLE_PAGE_CAPACITY, ProcessHandleSnapshot, TaskThreadSnapshot,
};
use crate::mm::user_space::{UserAddress, UserSlice};
use crate::process::ProcessSnapshot;
use hyper::abi::native::{
    HYPER_NATIVE_OBJECT_HANDLE_STATE_ACTIVE, HYPER_NATIVE_OBJECT_HANDLE_STATE_RETIRED,
    HYPER_NATIVE_OBJECT_HANDLE_STATE_UNPUBLISHED, HYPER_NATIVE_STATUS_INTERNAL,
    HYPER_NATIVE_STATUS_INVALID_ARGUMENT, HYPER_NATIVE_THREAD_REGISTRY_RESIDENT,
    HYPER_NATIVE_THREAD_REGISTRY_RETIRING, HYPER_NATIVE_THREAD_ROLE_BOOTSTRAP,
    HYPER_NATIVE_THREAD_ROLE_IDLE, HYPER_NATIVE_THREAD_ROLE_KERNEL, HYPER_NATIVE_THREAD_ROLE_USER,
    HYPER_NATIVE_THREAD_ROLE_VCPU, HyperNativeCpuObservation, HyperNativeHandleInspection,
    HyperNativeMemoryObservation, HyperNativeObjectInspection, HyperNativeStatus,
    HyperNativeTaskProcess, HyperNativeTaskThread,
};

pub(in crate::abi::native) fn parse_inspector_scan(
    arguments: &Arguments,
    capacity: usize,
    record_size: usize,
) -> Result<(u64, UserSlice), HyperNativeStatus> {
    let requested =
        usize::try_from(arguments[2]).map_err(|_| HYPER_NATIVE_STATUS_INVALID_ARGUMENT)?;
    if requested != capacity {
        return Err(HYPER_NATIVE_STATUS_INVALID_ARGUMENT);
    }
    let stride = validate_record_stride(arguments[3], record_size)?;
    let bytes = capacity
        .checked_mul(stride)
        .and_then(|bytes| u64::try_from(bytes).ok())
        .ok_or(HYPER_NATIVE_STATUS_INTERNAL)?;
    let destination =
        UserSlice::new(UserAddress::new(arguments[1]), bytes).map_err(status_from_address_error)?;
    Ok((arguments[0], destination))
}

pub(in crate::abi::native) fn parse_handle_inspector_scan(
    arguments: &Arguments,
) -> Result<(u64, u64, UserSlice), HyperNativeStatus> {
    let requested =
        usize::try_from(arguments[3]).map_err(|_| HYPER_NATIVE_STATUS_INVALID_ARGUMENT)?;
    if requested != HANDLE_PAGE_CAPACITY || arguments[0] == 0 {
        return Err(HYPER_NATIVE_STATUS_INVALID_ARGUMENT);
    }
    let stride = validate_record_stride(
        arguments[4],
        hyper::abi::native::HYPER_NATIVE_HANDLE_INSPECTION_MIN_SIZE,
    )?;
    let bytes = HANDLE_PAGE_CAPACITY
        .checked_mul(stride)
        .and_then(|bytes| u64::try_from(bytes).ok())
        .ok_or(HYPER_NATIVE_STATUS_INTERNAL)?;
    let destination =
        UserSlice::new(UserAddress::new(arguments[2]), bytes).map_err(status_from_address_error)?;
    Ok((arguments[0], arguments[1], destination))
}

pub(in crate::abi::native) fn encode_task_process(
    snapshot: ProcessSnapshot,
) -> [u8; core::mem::size_of::<HyperNativeTaskProcess>()] {
    type Record = HyperNativeTaskProcess;
    let mut record = [0_u8; core::mem::size_of::<Record>()];
    write_u64(
        &mut record,
        core::mem::offset_of!(Record, koid),
        snapshot.koid.get(),
    );
    write_u32(
        &mut record,
        core::mem::offset_of!(Record, phase),
        process_phase(snapshot.phase),
    );
    write_u32(
        &mut record,
        core::mem::offset_of!(Record, terminal_reason),
        terminal_reason(snapshot.terminal),
    );
    write_u32(
        &mut record,
        core::mem::offset_of!(Record, pending_threads),
        u32::try_from(snapshot.pending_threads).unwrap_or(u32::MAX),
    );
    write_u32(
        &mut record,
        core::mem::offset_of!(Record, active_threads),
        u32::try_from(snapshot.active_threads).unwrap_or(u32::MAX),
    );
    let name = snapshot.name.as_bytes();
    write_u32(
        &mut record,
        core::mem::offset_of!(Record, name_length),
        name.len() as u32,
    );
    let name_offset = core::mem::offset_of!(Record, name);
    record[name_offset..name_offset + name.len()].copy_from_slice(name);
    record
}

pub(in crate::abi::native) fn encode_task_thread(
    snapshot: TaskThreadSnapshot,
) -> [u8; core::mem::size_of::<HyperNativeTaskThread>()] {
    type Record = HyperNativeTaskThread;
    let mut record = [0_u8; core::mem::size_of::<Record>()];
    write_u64(
        &mut record,
        core::mem::offset_of!(Record, koid),
        snapshot.koid.get(),
    );
    write_u64(
        &mut record,
        core::mem::offset_of!(Record, process_koid),
        snapshot.process_koid.map_or(0, crate::object::Koid::get),
    );
    let role = match snapshot.role {
        crate::task::ThreadRole::Bootstrap => HYPER_NATIVE_THREAD_ROLE_BOOTSTRAP,
        crate::task::ThreadRole::Idle => HYPER_NATIVE_THREAD_ROLE_IDLE,
        crate::task::ThreadRole::Kernel => HYPER_NATIVE_THREAD_ROLE_KERNEL,
        crate::task::ThreadRole::User => HYPER_NATIVE_THREAD_ROLE_USER,
        crate::task::ThreadRole::Vcpu => HYPER_NATIVE_THREAD_ROLE_VCPU,
    };
    let registry_phase = match snapshot.registry_phase {
        crate::task::ThreadObjectRegistryPhase::Resident => HYPER_NATIVE_THREAD_REGISTRY_RESIDENT,
        crate::task::ThreadObjectRegistryPhase::Retiring => HYPER_NATIVE_THREAD_REGISTRY_RETIRING,
    };
    write_u32(
        &mut record,
        core::mem::offset_of!(Record, role),
        role as u32,
    );
    write_u32(
        &mut record,
        core::mem::offset_of!(Record, registry_phase),
        registry_phase as u32,
    );
    let name = snapshot.name.as_bytes();
    write_u32(
        &mut record,
        core::mem::offset_of!(Record, name_length),
        name.len() as u32,
    );
    write_u64(
        &mut record,
        core::mem::offset_of!(Record, runtime_ns),
        snapshot.runtime_ns,
    );
    let name_offset = core::mem::offset_of!(Record, name);
    record[name_offset..name_offset + name.len()].copy_from_slice(name);
    record
}

pub(in crate::abi::native) fn encode_memory_observation(
    snapshot: crate::diagnostics::inspect::MemoryObservation,
) -> [u8; core::mem::size_of::<HyperNativeMemoryObservation>()] {
    type Record = HyperNativeMemoryObservation;
    let mut record = [0_u8; core::mem::size_of::<Record>()];
    macro_rules! field {
        ($name:ident) => {
            write_u64(
                &mut record,
                core::mem::offset_of!(Record, $name),
                snapshot.$name,
            );
        };
    }
    field!(captured_at_ns);
    field!(page_size);
    field!(total_bytes);
    field!(managed_bytes);
    field!(free_bytes);
    field!(kernel_bytes);
    field!(slab_bytes);
    field!(large_heap_bytes);
    field!(page_table_bytes);
    field!(vmo_bytes);
    field!(guest_loaned_bytes);
    field!(guest_private_bytes);
    field!(file_cache_bytes);
    field!(slab_reclaimable_bytes);
    field!(slab_sample_complete);
    field!(file_cache_reclaimable_bytes);
    field!(file_cache_sample_complete);
    field!(ramfs_capacity_bytes);
    record
}

pub(in crate::abi::native) fn encode_cpu_observation(
    snapshot: crate::task::scheduler::CpuTimeSnapshot,
) -> [u8; core::mem::size_of::<HyperNativeCpuObservation>()] {
    type Record = HyperNativeCpuObservation;
    let mut record = [0_u8; core::mem::size_of::<Record>()];
    macro_rules! field {
        ($name:ident) => {
            write_u64(
                &mut record,
                core::mem::offset_of!(Record, $name),
                snapshot.$name,
            );
        };
    }
    field!(captured_at_ns);
    field!(reserved_clock);
    field!(online_cpus);
    field!(idle_ns);
    field!(kernel_thread_ns);
    field!(user_thread_ns);
    field!(vcpu_ns);
    write_u64(&mut record, core::mem::offset_of!(Record, reserved), 0);
    record
}

pub(in crate::abi::native) fn encode_object_inspection(
    snapshot: crate::object::ObjectSnapshot,
) -> [u8; core::mem::size_of::<HyperNativeObjectInspection>()] {
    type Record = HyperNativeObjectInspection;
    let mut record = [0_u8; core::mem::size_of::<Record>()];
    write_u64(
        &mut record,
        core::mem::offset_of!(Record, koid),
        snapshot.koid.get(),
    );
    write_u32(
        &mut record,
        core::mem::offset_of!(Record, object_kind),
        snapshot.kind.get(),
    );
    let (state, active) = match snapshot.handles {
        crate::object::ObjectHandleState::Unpublished => {
            (HYPER_NATIVE_OBJECT_HANDLE_STATE_UNPUBLISHED, 0)
        }
        crate::object::ObjectHandleState::Active(count) => (
            HYPER_NATIVE_OBJECT_HANDLE_STATE_ACTIVE,
            u64::try_from(count).unwrap_or(u64::MAX),
        ),
        crate::object::ObjectHandleState::Retired => (HYPER_NATIVE_OBJECT_HANDLE_STATE_RETIRED, 0),
    };
    write_u32(
        &mut record,
        core::mem::offset_of!(Record, handle_state),
        state as u32,
    );
    write_u64(
        &mut record,
        core::mem::offset_of!(Record, active_handles),
        active,
    );
    write_u64(
        &mut record,
        core::mem::offset_of!(Record, supported_rights),
        snapshot.supported_rights.bits(),
    );
    write_u64(
        &mut record,
        core::mem::offset_of!(Record, strong_references),
        u64::try_from(snapshot.strong_references).unwrap_or(u64::MAX),
    );
    write_u64(
        &mut record,
        core::mem::offset_of!(Record, kernel_service_references),
        usize_to_u64(snapshot.references.kernel_service),
    );
    write_u64(
        &mut record,
        core::mem::offset_of!(Record, vm_device_binding_references),
        usize_to_u64(snapshot.references.vm_device_binding),
    );
    write_u64(
        &mut record,
        core::mem::offset_of!(Record, scheduler_references),
        usize_to_u64(snapshot.references.scheduler),
    );
    write_u64(
        &mut record,
        core::mem::offset_of!(Record, operation_references),
        usize_to_u64(snapshot.references.operation_pin),
    );
    write_u64(
        &mut record,
        core::mem::offset_of!(Record, user_authority_references),
        usize_to_u64(snapshot.references.user_authority),
    );
    write_u64(
        &mut record,
        core::mem::offset_of!(Record, publication_references),
        usize_to_u64(snapshot.references.publication),
    );
    write_u64(
        &mut record,
        core::mem::offset_of!(Record, diagnostic_references),
        usize_to_u64(snapshot.references.diagnostic),
    );
    write_u64(
        &mut record,
        core::mem::offset_of!(Record, retirement_references),
        usize_to_u64(snapshot.references.retirement),
    );
    record
}

pub(in crate::abi::native) fn encode_handle_inspection(
    snapshot: ProcessHandleSnapshot,
) -> [u8; core::mem::size_of::<HyperNativeHandleInspection>()] {
    type Record = HyperNativeHandleInspection;
    let mut record = [0_u8; core::mem::size_of::<Record>()];
    write_u64(
        &mut record,
        core::mem::offset_of!(Record, process_koid),
        snapshot.process_koid.get(),
    );
    write_u64(
        &mut record,
        core::mem::offset_of!(Record, handle),
        handle_wire::to_abi(snapshot.handle.value),
    );
    write_u64(
        &mut record,
        core::mem::offset_of!(Record, object_koid),
        snapshot.handle.info.koid.get(),
    );
    write_u64(
        &mut record,
        core::mem::offset_of!(Record, rights),
        snapshot.handle.info.rights.bits(),
    );
    write_u32(
        &mut record,
        core::mem::offset_of!(Record, object_kind),
        snapshot.handle.info.kind.get(),
    );
    write_u32(
        &mut record,
        core::mem::offset_of!(Record, flags),
        snapshot.handle.info.flags.bits(),
    );
    record
}
