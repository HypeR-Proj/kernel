// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Native syscall routing in ordinary Thread context.

use hyper::abi::native::{self as abi, NativeInvocation};

use super::handlers;
use super::services::{NativeAction, NativeServices};

/// Dispatches an owned invocation in ordinary, interruptible Thread context.
///
/// Services may sleep before producing an action. The caller owns scheduling
/// and Process lifecycle effects; architecture frames remain private to HAL.
pub(crate) fn dispatch(
    services: &impl NativeServices,
    invocation: NativeInvocation,
) -> NativeAction {
    let (receiver, operation) = match admit_invocation(services, &invocation) {
        Ok(authorized) => authorized,
        Err(status) => return NativeAction::Return(super::status::failure(status)),
    };
    dispatch_operation(services, &receiver, operation, invocation.arguments())
}

/// Resolves the receiver and validates method authority before reading payloads.
pub(super) fn admit_invocation(
    services: &impl super::services::HandleServices,
    invocation: &NativeInvocation,
) -> Result<(crate::capability::InvocationReceiver, abi::NativeOperation), abi::HyperNativeStatus> {
    let handle = super::wire::parse_handle(invocation.target())?;
    let receiver = services
        .resolve_invocation(handle)
        .map_err(super::status::status_from_process_error)?;
    let operation = abi::NativeOperation::from_raw(receiver.kind().get(), invocation.operation())
        .ok_or(abi::HYPER_NATIVE_STATUS_NOT_SUPPORTED)?;
    let rights = crate::capability::Rights::from_bits(operation.receiver_rights())
        .ok_or(abi::HYPER_NATIVE_STATUS_INTERNAL)?;
    receiver
        .require(rights)
        .map_err(super::status::status_from_handle_error)?;
    services
        .validate_invocation_context(&receiver)
        .map_err(super::status::status_from_process_error)?;
    Ok((receiver, operation))
}

/// Routes an authorized operation without repeating handle lookup or encoding
/// the receiver back into the business argument registers.
pub(super) fn dispatch_operation(
    services: &impl NativeServices,
    receiver: &crate::capability::InvocationReceiver,
    operation: abi::NativeOperation,
    arguments: &super::Arguments,
) -> NativeAction {
    if arguments[operation.argument_count()..]
        .iter()
        .any(|value| *value != 0)
    {
        return NativeAction::Return(super::status::failure(
            abi::HYPER_NATIVE_STATUS_INVALID_ARGUMENT,
        ));
    }
    #[cfg(feature = "kernel-trace")]
    let trace_call = !matches!(
        operation,
        abi::NativeOperation::CpuInspectorTraceStart
            | abi::NativeOperation::CpuInspectorTraceStop
            | abi::NativeOperation::CpuInspectorTraceClear
            | abi::NativeOperation::CpuInspectorTraceRead
            | abi::NativeOperation::ByteChannelTraceRpc
    );
    #[cfg(feature = "kernel-trace")]
    if trace_call {
        crate::tracepoint!(
            Syscall,
            "syscall_enter",
            thread = services.trace_thread_id(),
            process = services.current_process_id(),
            kind = receiver.kind().get() as u64,
            operation = operation.operation()
        );
    }
    let action = match operation {
        abi::NativeOperation::ThreadGetCurrentScheduling
        | abi::NativeOperation::ThreadGetScheduling
        | abi::NativeOperation::VirtualCpuGetScheduling => {
            NativeAction::Return(handlers::sys_get_scheduling(services, receiver, arguments))
        }
        abi::NativeOperation::ThreadSetCurrentScheduling
        | abi::NativeOperation::ThreadSetScheduling
        | abi::NativeOperation::VirtualCpuSetScheduling => NativeAction::Return(
            handlers::sys_set_scheduling(services, receiver, arguments, false),
        ),
        abi::NativeOperation::ProcessBuilderSetScheduling => NativeAction::Return(
            handlers::sys_set_scheduling(services, receiver, arguments, true),
        ),
        abi::NativeOperation::AbiQuery => NativeAction::Return(handlers::sys_abi_query(arguments)),
        abi::NativeOperation::SystemConfig => {
            NativeAction::Return(handlers::sys_system_config(arguments))
        }
        abi::NativeOperation::ClockGetMonotonic => {
            NativeAction::Return(handlers::sys_clock_get_monotonic(arguments))
        }
        abi::NativeOperation::HandleClose => {
            NativeAction::Return(handlers::sys_handle_close(services, receiver, arguments))
        }
        abi::NativeOperation::ProcessDeriveObserver => {
            handlers::sys_process_derive_observer(services, receiver, arguments)
        }
        abi::NativeOperation::TaskGroupRequestStop => {
            handlers::sys_task_group_request_stop(services, receiver, arguments)
        }
        abi::NativeOperation::ConsoleDiscardInput => {
            handlers::sys_console_discard_input(services, receiver, arguments)
        }
        abi::NativeOperation::HandleGetInfo => {
            NativeAction::Return(handlers::sys_handle_get_info(services, receiver, arguments))
        }
        abi::NativeOperation::ObjectGetBasicInfo => NativeAction::Return(
            handlers::sys_object_get_basic_info(services, receiver, arguments),
        ),
        abi::NativeOperation::VmoCreateSnapshot => {
            handlers::sys_vmo_create_snapshot(services, receiver, arguments)
        }
        abi::NativeOperation::FileProtect => {
            handlers::sys_file_protect(services, receiver, arguments)
        }
        abi::NativeOperation::VmarLockMemory => {
            handlers::sys_vmar_lock_memory(services, receiver, arguments)
        }
        abi::NativeOperation::VmarUnlockMemory => {
            handlers::sys_vmar_unlock_memory(services, receiver, arguments)
        }
        abi::NativeOperation::VmarMapFile => {
            handlers::sys_vmar_map_file(services, receiver, arguments)
        }
        abi::NativeOperation::VmarMapPrivate => {
            handlers::sys_vmar_map_private(services, receiver, arguments)
        }
        abi::NativeOperation::HandleDuplicate => NativeAction::Return(
            handlers::sys_handle_duplicate(services, receiver, arguments),
        ),
        abi::NativeOperation::HandleReplace => {
            NativeAction::Return(handlers::sys_handle_replace(services, receiver, arguments))
        }
        abi::NativeOperation::EventCreate => {
            NativeAction::Return(handlers::sys_event_create(services, arguments))
        }
        abi::NativeOperation::ByteChannelCreate => {
            NativeAction::Return(handlers::sys_byte_channel_create(services, arguments))
        }
        abi::NativeOperation::CapabilityChannelCreate => {
            NativeAction::Return(handlers::sys_capability_channel_create(services, arguments))
        }
        abi::NativeOperation::ThreadSetCurrentName => {
            NativeAction::Return(handlers::sys_thread_set_current_name(services, arguments))
        }
        abi::NativeOperation::ThreadCreate => {
            NativeAction::Return(handlers::sys_thread_create(services, receiver, arguments))
        }
        abi::NativeOperation::ThreadStart => {
            NativeAction::Return(handlers::sys_thread_start(services, receiver, arguments))
        }
        abi::NativeOperation::ThreadRequestStop => NativeAction::Return(
            handlers::sys_thread_request_stop(services, receiver, arguments),
        ),
        abi::NativeOperation::AtomicWait => {
            NativeAction::Return(handlers::sys_atomic_wait(services, arguments))
        }
        abi::NativeOperation::AtomicWake => {
            NativeAction::Return(handlers::sys_atomic_wake(services, arguments))
        }
        abi::NativeOperation::ThreadSleep => {
            NativeAction::Return(handlers::sys_thread_sleep(services, arguments))
        }
        abi::NativeOperation::ThreadYield => handlers::sys_thread_yield(),
        abi::NativeOperation::ThreadExit => handlers::sys_thread_exit(arguments),
        abi::NativeOperation::ProcessExit => handlers::sys_process_exit(arguments),
        abi::NativeOperation::EventSignal => {
            handlers::sys_event_signal(services, receiver, arguments)
        }
        abi::NativeOperation::ObjectWaitOne => {
            handlers::sys_object_wait_one(services, receiver, arguments)
        }
        abi::NativeOperation::ObjectWaitMany => handlers::sys_object_wait_many(services, arguments),
        abi::NativeOperation::ByteChannelWrite => {
            handlers::sys_byte_channel_write(services, receiver, arguments)
        }
        abi::NativeOperation::ByteChannelTraceRpc => {
            handlers::sys_byte_channel_trace_rpc(services, receiver, arguments)
        }
        abi::NativeOperation::ByteChannelRead => {
            handlers::sys_byte_channel_read(services, receiver, arguments)
        }
        abi::NativeOperation::CapabilityChannelTrySend => {
            handlers::sys_capability_channel_try_send(services, receiver, arguments)
        }
        abi::NativeOperation::CapabilityChannelReceive => {
            handlers::sys_capability_channel_receive(services, receiver, arguments)
        }
        abi::NativeOperation::ConsoleRead => {
            handlers::sys_console_read(services, receiver, arguments)
        }
        abi::NativeOperation::ConsoleWrite => {
            handlers::sys_console_write(services, receiver, arguments)
        }
        abi::NativeOperation::DirectoryOpenFile => {
            handlers::sys_directory_open_file(services, receiver, arguments)
        }
        abi::NativeOperation::DirectoryOpenDirectory => {
            handlers::sys_directory_open_directory(services, receiver, arguments)
        }
        abi::NativeOperation::DirectoryBeginRead => {
            handlers::sys_directory_begin_read(services, receiver, arguments)
        }
        abi::NativeOperation::DirectoryRead => {
            handlers::sys_directory_read(services, receiver, arguments)
        }
        abi::NativeOperation::FileWriteAt => {
            handlers::sys_file_write_at(services, receiver, arguments)
        }
        abi::NativeOperation::FileResize => {
            handlers::sys_file_resize(services, receiver, arguments)
        }
        abi::NativeOperation::DirectoryCreateFile => {
            handlers::sys_directory_create_file(services, receiver, arguments)
        }
        abi::NativeOperation::DirectoryCreateDirectory => {
            handlers::sys_directory_create_directory(services, receiver, arguments)
        }
        abi::NativeOperation::DirectoryScopeCreate => {
            handlers::sys_directory_scope_create(services, receiver, arguments)
        }
        abi::NativeOperation::DirectoryGetMetadata => {
            handlers::sys_directory_get_metadata(services, receiver, arguments)
        }
        abi::NativeOperation::FileGetMetadata => {
            handlers::sys_file_get_metadata(services, receiver, arguments)
        }
        abi::NativeOperation::DirectoryGetSelfMetadata => {
            handlers::sys_directory_get_self_metadata(services, receiver, arguments)
        }
        abi::NativeOperation::DirectorySetMetadata => {
            handlers::sys_directory_set_metadata(services, receiver, arguments)
        }
        abi::NativeOperation::FileSetMetadata => {
            handlers::sys_file_set_metadata(services, receiver, arguments)
        }
        abi::NativeOperation::DirectoryRename => {
            handlers::sys_directory_rename(services, receiver, arguments)
        }
        abi::NativeOperation::DirectoryLink => {
            handlers::sys_directory_link(services, receiver, arguments)
        }
        abi::NativeOperation::DirectorySymlink => {
            handlers::sys_directory_symlink(services, receiver, arguments)
        }
        abi::NativeOperation::DirectoryReadLink => {
            handlers::sys_directory_read_link(services, receiver, arguments)
        }
        abi::NativeOperation::DirectoryCanonicalize => {
            handlers::sys_directory_canonicalize(services, receiver, arguments)
        }
        abi::NativeOperation::DirectoryRemoveIf => {
            handlers::sys_directory_remove_if(services, receiver, arguments)
        }
        abi::NativeOperation::DirectoryOpenDirectoryNofollow => {
            handlers::sys_directory_open_directory_nofollow(services, receiver, arguments)
        }
        abi::NativeOperation::FileSync => handlers::sys_file_sync(services, receiver, arguments),
        abi::NativeOperation::FileLock => handlers::sys_file_lock(services, receiver, arguments),
        abi::NativeOperation::FileUnlock => {
            handlers::sys_file_unlock(services, receiver, arguments)
        }
        abi::NativeOperation::DirectoryOpenFileWithOptions => {
            handlers::sys_directory_open_file_with_options(services, receiver, arguments)
        }
        abi::NativeOperation::ClockGetRealtime => handlers::sys_clock_get_realtime(arguments),
        abi::NativeOperation::DirectoryRemove => {
            handlers::sys_directory_remove(services, receiver, arguments)
        }
        abi::NativeOperation::WaitSetCreate => handlers::sys_wait_set_create(services, arguments),
        abi::NativeOperation::WaitSetAdd => {
            handlers::sys_wait_set_add(services, receiver, arguments)
        }
        abi::NativeOperation::WaitSetRearm => {
            handlers::sys_wait_set_rearm(services, receiver, arguments)
        }
        abi::NativeOperation::WaitSetRemove => {
            handlers::sys_wait_set_remove(services, receiver, arguments)
        }
        abi::NativeOperation::WaitSetWait => {
            handlers::sys_wait_set_wait(services, receiver, arguments)
        }
        abi::NativeOperation::ProcessGetCurrentId => {
            handlers::sys_process_get_current_id(services, arguments)
        }
        abi::NativeOperation::FileReadAt => {
            handlers::sys_file_read_at(services, receiver, arguments)
        }
        abi::NativeOperation::FileGetInfo => {
            handlers::sys_file_get_info(services, receiver, arguments)
        }
        abi::NativeOperation::DirectoryGetInfo => {
            handlers::sys_directory_get_info(services, receiver, arguments)
        }
        abi::NativeOperation::VirtualMachineCreationLeaseCreate => {
            handlers::sys_virtual_machine_creation_lease_create(services, receiver, arguments)
        }
        abi::NativeOperation::VirtualMachineCreate => {
            handlers::sys_virtual_machine_create(services, receiver, arguments)
        }
        abi::NativeOperation::PendingVirtualMachineSetMemory => {
            handlers::sys_pending_virtual_machine_set_memory(services, receiver, arguments)
        }
        abi::NativeOperation::PendingVirtualMachineSetBootstrap => {
            handlers::sys_pending_virtual_machine_set_bootstrap(services, receiver, arguments)
        }
        abi::NativeOperation::PendingVirtualMachineSetVirtualSerial => {
            handlers::sys_pending_virtual_machine_set_virtual_serial(services, receiver, arguments)
        }
        abi::NativeOperation::VirtualSerialCreate => {
            handlers::sys_virtual_serial_create(services, arguments)
        }
        abi::NativeOperation::VirtualSerialRegisterOutput => {
            handlers::sys_virtual_serial_register_output(services, receiver, arguments)
        }
        abi::NativeOperation::VirtualSerialAcknowledgeOutput => {
            handlers::sys_virtual_serial_acknowledge_output(services, receiver, arguments)
        }
        abi::NativeOperation::VirtualSerialWrite => {
            handlers::sys_virtual_serial_write(services, receiver, arguments)
        }
        abi::NativeOperation::PendingVirtualMachineSeal => {
            handlers::sys_pending_virtual_machine_seal(services, receiver, arguments)
        }
        abi::NativeOperation::PendingVirtualMachineInstall => {
            handlers::sys_pending_virtual_machine_install(services, receiver, arguments)
        }
        abi::NativeOperation::PendingVirtualMachineAbort => {
            handlers::sys_pending_virtual_machine_abort(services, receiver, arguments)
        }
        abi::NativeOperation::VirtualMachineRequestStop => {
            handlers::sys_virtual_machine_request_stop(services, receiver, arguments)
        }
        abi::NativeOperation::VirtualMachineCreationLeaseGetPlatformInfo => {
            handlers::sys_virtual_machine_creation_lease_get_platform_info(
                services, receiver, arguments,
            )
        }
        abi::NativeOperation::VirtualMachineGetPowerRequest => {
            handlers::sys_virtual_machine_get_power_request(services, receiver, arguments)
        }
        abi::NativeOperation::VirtualMachineCompletePowerRequest => {
            handlers::sys_virtual_machine_complete_power_request(services, receiver, arguments)
        }
        abi::NativeOperation::VirtualMachineOpenVcpu => {
            handlers::sys_virtual_machine_open_vcpu(services, receiver, arguments)
        }
        abi::NativeOperation::VirtualMachineGetInfo => {
            handlers::sys_virtual_machine_get_info(services, receiver, arguments)
        }
        abi::NativeOperation::VirtualMachineRegisterMmio => {
            handlers::sys_virtual_machine_register_mmio(services, receiver, arguments)
        }
        abi::NativeOperation::VirtualCpuGetMmioRequest => {
            handlers::sys_virtual_cpu_get_mmio_request(services, receiver, arguments)
        }
        abi::NativeOperation::VirtualCpuCompleteMmio => {
            handlers::sys_virtual_cpu_complete_mmio(services, receiver, arguments)
        }
        abi::NativeOperation::GuestMemoryTransfer => {
            handlers::sys_guest_memory_transfer(services, receiver, arguments)
        }
        abi::NativeOperation::GuestMemoryScalar => {
            handlers::sys_guest_memory_scalar(services, receiver, arguments)
        }
        abi::NativeOperation::GuestInterruptCreate => {
            handlers::sys_guest_interrupt_create(services, receiver, arguments)
        }
        abi::NativeOperation::GuestInterruptSetLevel => {
            handlers::sys_guest_interrupt_set_level(services, receiver, arguments)
        }
        abi::NativeOperation::GuestMemoryCreate => {
            handlers::sys_guest_memory_create(services, receiver, arguments)
        }
        abi::NativeOperation::PendingVirtualMachineMapMemory => {
            handlers::sys_pending_virtual_machine_map_memory(services, receiver, arguments)
        }
        abi::NativeOperation::DeviceFirmwareRead => {
            handlers::sys_device_firmware_read(services, receiver, arguments)
        }
        abi::NativeOperation::DeviceClaimBundle => {
            handlers::sys_device_claim_bundle(services, receiver, arguments)
        }
        abi::NativeOperation::DeviceMmio => {
            handlers::sys_device_mmio(services, receiver, arguments)
        }
        abi::NativeOperation::DeviceIrqPending => {
            handlers::sys_device_irq_pending(services, receiver, arguments)
        }
        abi::NativeOperation::DeviceIrqComplete => {
            handlers::sys_device_irq_complete(services, receiver, arguments)
        }
        abi::NativeOperation::DeviceDmaFaultInfo => {
            handlers::sys_device_dma_fault_info(services, receiver, arguments)
        }
        abi::NativeOperation::DeviceProfileInfo => {
            handlers::sys_device_profile_info(services, receiver, arguments)
        }
        abi::NativeOperation::DeviceResourceInfo => {
            handlers::sys_device_resource_info(services, receiver, arguments)
        }
        abi::NativeOperation::DeviceClaimMatching => {
            handlers::sys_device_claim_matching(services, receiver, arguments)
        }
        abi::NativeOperation::DeviceClaim => {
            handlers::sys_device_claim(services, receiver, arguments)
        }
        abi::NativeOperation::PhysicalDeviceInfo => {
            handlers::sys_physical_device_info(services, receiver, arguments)
        }
        abi::NativeOperation::VmoGetDmaExtent => {
            handlers::sys_vmo_get_dma_extent(services, receiver, arguments)
        }
        abi::NativeOperation::PendingVirtualMachineAssignDevice => {
            handlers::sys_pending_virtual_machine_assign_device(services, receiver, arguments)
        }
        abi::NativeOperation::GuestMailboxCreate => {
            handlers::sys_guest_mailbox_create(services, receiver, arguments)
        }
        abi::NativeOperation::GuestMailboxSend => {
            handlers::sys_guest_mailbox_send(services, receiver, arguments)
        }
        abi::NativeOperation::GuestMailboxReceive => {
            handlers::sys_guest_mailbox_receive(services, receiver, arguments)
        }
        abi::NativeOperation::GuestNotificationCreate => {
            handlers::sys_guest_notification_create(services, receiver, arguments)
        }
        abi::NativeOperation::GuestMappingCreate => {
            handlers::sys_guest_mapping_create(services, receiver, arguments)
        }
        abi::NativeOperation::GuestMappingRelease => {
            handlers::sys_guest_mapping_release(services, receiver, arguments)
        }
        abi::NativeOperation::NativeBlockCreate => {
            handlers::sys_native_block_create(services, receiver, arguments)
        }
        abi::NativeOperation::NativeBlockActivate => {
            handlers::sys_native_block_activate(services, receiver, arguments)
        }
        abi::NativeOperation::NativeBlockTransfer => {
            handlers::sys_native_block_transfer(services, receiver, arguments)
        }
        abi::NativeOperation::BlockQueueCreate => {
            handlers::sys_block_queue_create(services, receiver, arguments)
        }
        abi::NativeOperation::BlockQueueSubmit => {
            handlers::sys_block_queue_submit(services, receiver, arguments)
        }
        abi::NativeOperation::BlockQueueCreateWindow => {
            handlers::sys_block_queue_create_window(services, receiver, arguments)
        }
        abi::NativeOperation::BlockQueueSubmitWindow => {
            handlers::sys_block_queue_submit_window(services, receiver, arguments)
        }
        abi::NativeOperation::BlockQueueWait => {
            handlers::sys_block_queue_wait(services, receiver, arguments)
        }
        abi::NativeOperation::BlockQueueWaitSpace => {
            handlers::sys_block_queue_wait_space(services, receiver, arguments)
        }
        abi::NativeOperation::BlockQueueReap => {
            handlers::sys_block_queue_reap(services, receiver, arguments)
        }
        abi::NativeOperation::BlockQueueInfo => {
            handlers::sys_block_queue_info(services, receiver, arguments)
        }
        abi::NativeOperation::FilesystemMountRamfs => {
            handlers::sys_filesystem_mount_ramfs(services, receiver, arguments)
        }
        abi::NativeOperation::FilesystemSwitchRoot => {
            handlers::sys_filesystem_switch_root(services, receiver, arguments)
        }
        abi::NativeOperation::FilesystemBeginRootSwitch => {
            handlers::sys_filesystem_begin_root_switch(services, receiver, arguments)
        }
        abi::NativeOperation::FilesystemFinishRootSwitch => {
            handlers::sys_filesystem_finish_root_switch(services, receiver, arguments)
        }
        abi::NativeOperation::FilesystemUnmount => {
            handlers::sys_filesystem_unmount(services, receiver, arguments)
        }
        abi::NativeOperation::FilesystemMount => {
            handlers::sys_filesystem_mount(services, receiver, arguments)
        }
        abi::NativeOperation::GuestNotificationAccess => {
            handlers::sys_guest_notification_access(services, receiver, arguments)
        }
        abi::NativeOperation::GuestNotificationConfigure => {
            handlers::sys_guest_notification_configure(services, receiver, arguments)
        }
        abi::NativeOperation::GuestNotificationControl => {
            handlers::sys_guest_notification_control(services, receiver, arguments)
        }
        abi::NativeOperation::VmoCreateContiguous => {
            handlers::sys_vmo_create_contiguous(services, arguments)
        }
        abi::NativeOperation::VirtualCpuGetInfo => {
            handlers::sys_virtual_cpu_get_info(services, receiver, arguments)
        }
        abi::NativeOperation::VirtualCpuSetAffinity => {
            handlers::sys_virtual_cpu_set_affinity(services, receiver, arguments)
        }
        abi::NativeOperation::VirtualCpuStart => {
            handlers::sys_virtual_cpu_start(services, receiver, arguments)
        }
        abi::NativeOperation::ResourceDomainCreate => {
            handlers::sys_resource_domain_create(services, receiver, arguments)
        }
        abi::NativeOperation::TaskGroupCreate => {
            handlers::sys_task_group_create(services, receiver, arguments)
        }
        abi::NativeOperation::VmoCreate => handlers::sys_vmo_create(services, arguments),
        abi::NativeOperation::VmoRead => handlers::sys_vmo_read(services, receiver, arguments),
        abi::NativeOperation::VmoWrite => handlers::sys_vmo_write(services, receiver, arguments),
        abi::NativeOperation::VmarAllocate => {
            handlers::sys_vmar_allocate(services, receiver, arguments)
        }
        abi::NativeOperation::VmarMap => handlers::sys_vmar_map(services, receiver, arguments),
        abi::NativeOperation::VmarProtect => {
            handlers::sys_vmar_protect(services, receiver, arguments)
        }
        abi::NativeOperation::VmarUnmap => handlers::sys_vmar_unmap(services, receiver, arguments),
        abi::NativeOperation::VmarDestroy => {
            handlers::sys_vmar_destroy(services, receiver, arguments)
        }
        abi::NativeOperation::ProcessBuilderRetain => {
            handlers::sys_process_builder_retain(services, receiver, arguments)
        }
        abi::NativeOperation::ProcessRetirementOpenView => {
            handlers::sys_process_retirement_open_view(services, receiver, arguments)
        }
        abi::NativeOperation::ProcessRetirementRelease => {
            handlers::sys_process_retirement_release(services, receiver, arguments)
        }
        abi::NativeOperation::CrashViewRead => {
            handlers::sys_crash_view_read(services, receiver, arguments)
        }
        abi::NativeOperation::CrashViewThread => {
            handlers::sys_crash_view_thread(services, receiver, arguments)
        }
        abi::NativeOperation::CrashViewMapping => {
            handlers::sys_crash_view_mapping(services, receiver, arguments)
        }
        abi::NativeOperation::CrashViewHandle => {
            handlers::sys_crash_view_handle(services, receiver, arguments)
        }
        abi::NativeOperation::ProcessRetirementGetInfo => {
            handlers::sys_retention_info(services, receiver, arguments, false)
        }
        abi::NativeOperation::CrashViewGetInfo => {
            handlers::sys_retention_info(services, receiver, arguments, true)
        }
        abi::NativeOperation::ProcessBuilderCreate => {
            handlers::sys_process_builder_create(services, receiver, arguments)
        }
        abi::NativeOperation::ProcessBuilderSetName => {
            handlers::sys_process_builder_set_name(services, receiver, arguments)
        }
        abi::NativeOperation::ProcessBuilderSetData => {
            handlers::sys_process_builder_set_data(services, receiver, arguments)
        }
        abi::NativeOperation::ProcessBuilderSetAffinity => {
            handlers::sys_process_builder_set_affinity(services, receiver, arguments)
        }
        abi::NativeOperation::ProcessBuilderSetMemoryLock => {
            handlers::sys_process_builder_set_memory_lock(services, receiver, arguments)
        }
        abi::NativeOperation::ProcessBuilderAddHandle => {
            handlers::sys_process_builder_add_handle(services, receiver, arguments)
        }
        abi::NativeOperation::ProcessBuilderSeal => {
            handlers::sys_process_builder_seal(services, receiver, arguments)
        }
        abi::NativeOperation::ProcessBuilderStart => {
            handlers::sys_process_builder_start(services, receiver, arguments)
        }
        abi::NativeOperation::ProcessBuilderAbort => {
            handlers::sys_process_builder_abort(services, receiver, arguments)
        }
        abi::NativeOperation::ProcessRequestStop => {
            handlers::sys_process_request_stop(services, receiver, arguments)
        }
        abi::NativeOperation::ProcessGetInfo => {
            handlers::sys_process_get_info(services, receiver, arguments)
        }
        abi::NativeOperation::TaskInspectorScanProcesses => {
            handlers::sys_task_inspector_scan_processes(services, receiver, arguments)
        }
        abi::NativeOperation::TaskInspectorScanThreads => {
            handlers::sys_task_inspector_scan_threads(services, receiver, arguments)
        }
        abi::NativeOperation::TaskInspectorDeriveProcess => {
            handlers::sys_task_inspector_derive_process(services, receiver, arguments)
        }
        abi::NativeOperation::ObjectInspectorReadDetails => {
            handlers::sys_object_inspector_read_details(services, receiver, arguments)
        }
        abi::NativeOperation::ObjectInspectorScanObjects => {
            handlers::sys_object_inspector_scan_objects(services, receiver, arguments)
        }
        abi::NativeOperation::ObjectInspectorScanHandles => {
            handlers::sys_object_inspector_scan_handles(services, receiver, arguments)
        }
        abi::NativeOperation::ObjectInspectorDeriveProcess => {
            handlers::sys_object_inspector_derive_process(services, receiver, arguments)
        }
        abi::NativeOperation::TaskInspectorDeriveTaskGroup => {
            handlers::sys_task_inspector_derive_task_group(services, receiver, arguments)
        }
        abi::NativeOperation::ObjectInspectorDeriveTaskGroup => {
            handlers::sys_object_inspector_derive_task_group(services, receiver, arguments)
        }
        abi::NativeOperation::TaskInspectorDeriveResourceDomain => {
            handlers::sys_task_inspector_derive_resource_domain(services, receiver, arguments)
        }
        abi::NativeOperation::ObjectInspectorDeriveResourceDomain => {
            handlers::sys_object_inspector_derive_resource_domain(services, receiver, arguments)
        }
        abi::NativeOperation::MemoryInspectorReadStatistics => {
            handlers::sys_memory_inspector_read_statistics(services, receiver, arguments)
        }
        abi::NativeOperation::MemoryInspectorRead => {
            handlers::sys_memory_inspector_read(services, receiver, arguments)
        }
        abi::NativeOperation::CpuInspectorProfileStart => {
            handlers::sys_cpu_inspector_profile_start(services, receiver, arguments)
        }
        abi::NativeOperation::CpuInspectorProfileStop => {
            handlers::sys_cpu_inspector_profile_stop(services, receiver, arguments)
        }
        abi::NativeOperation::CpuInspectorProfileRead => {
            handlers::sys_cpu_inspector_profile_read(services, receiver, arguments)
        }
        abi::NativeOperation::CpuInspectorTraceStart => {
            handlers::sys_cpu_inspector_trace_start(services, receiver, arguments)
        }
        abi::NativeOperation::CpuInspectorTraceStop => {
            handlers::sys_cpu_inspector_trace_stop(services, receiver, arguments)
        }
        abi::NativeOperation::CpuInspectorTraceClear => {
            handlers::sys_cpu_inspector_trace_clear(services, receiver, arguments)
        }
        abi::NativeOperation::CpuInspectorTraceRead => {
            handlers::sys_cpu_inspector_trace_read(services, receiver, arguments)
        }
        abi::NativeOperation::CpuInspectorRead => {
            handlers::sys_cpu_inspector_read(services, receiver, arguments)
        }
    };
    #[cfg(feature = "kernel-trace")]
    if trace_call {
        crate::tracepoint!(
            Syscall,
            "syscall_exit",
            thread = services.trace_thread_id(),
            status = match &action {
                NativeAction::Return(result) | NativeAction::Yield(result) =>
                    result.status() as u64,
                NativeAction::ExitThread { status } | NativeAction::ExitProcess { status } =>
                    *status as u64,
            },
            returned = u64::from(matches!(
                &action,
                NativeAction::Return(_) | NativeAction::Yield(_)
            ))
        );
    }
    action
}
