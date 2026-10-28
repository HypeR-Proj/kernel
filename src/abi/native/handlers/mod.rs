// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Native syscall leaves grouped by the capability domain they validate.
//!
//! Keep each syscall as a distinct machine frame: routing must not inherit
//! the largest handler stack, and crash traces must identify the active call.

mod block_queue;
mod console;
mod device;
mod guest_io;
mod handles;
mod inspect;
mod ipc;
mod memory;
mod memory_statistics;
pub(super) use memory_statistics::sys_memory_inspector_read_statistics;
mod object;
mod process_builder;
mod retention;
pub(super) use retention::{
    sys_crash_view_handle, sys_crash_view_mapping, sys_crash_view_read, sys_crash_view_thread,
    sys_process_builder_retain, sys_process_retirement_open_view, sys_process_retirement_release,
    sys_retention_info,
};
mod scheduling;
pub(super) use scheduling::{sys_get_scheduling, sys_set_scheduling};
mod system;
mod task;
mod vfs;
mod vm;

pub(super) use block_queue::{
    sys_block_queue_create, sys_block_queue_create_window, sys_block_queue_info,
    sys_block_queue_reap, sys_block_queue_submit, sys_block_queue_submit_window,
    sys_block_queue_wait, sys_block_queue_wait_space,
};
pub(super) use console::{sys_console_discard_input, sys_console_read, sys_console_write};
pub(super) use device::{
    sys_device_claim, sys_device_claim_bundle, sys_device_claim_matching,
    sys_device_dma_fault_info, sys_device_firmware_read, sys_device_irq_complete,
    sys_device_irq_pending, sys_device_mmio, sys_device_profile_info, sys_device_resource_info,
    sys_pending_virtual_machine_assign_device, sys_physical_device_info, sys_vmo_get_dma_extent,
};
pub(super) use guest_io::{
    sys_filesystem_mount, sys_filesystem_unmount, sys_guest_interrupt_create,
    sys_guest_interrupt_set_level, sys_guest_mailbox_create, sys_guest_mailbox_receive,
    sys_guest_mailbox_send, sys_guest_mapping_create, sys_guest_mapping_release,
    sys_guest_memory_scalar, sys_guest_memory_transfer, sys_guest_notification_access,
    sys_guest_notification_configure, sys_guest_notification_control,
    sys_guest_notification_create, sys_native_block_activate, sys_native_block_create,
    sys_native_block_transfer,
};
pub(super) use handles::{
    sys_handle_close, sys_handle_duplicate, sys_handle_get_info, sys_handle_replace,
    sys_object_get_basic_info,
};
pub(super) use inspect::{
    sys_cpu_inspector_read, sys_memory_inspector_read, sys_object_inspector_derive_process,
    sys_object_inspector_derive_resource_domain, sys_object_inspector_derive_task_group,
    sys_object_inspector_read_details, sys_object_inspector_scan_handles,
    sys_object_inspector_scan_objects, sys_task_inspector_derive_process,
    sys_task_inspector_derive_resource_domain, sys_task_inspector_derive_task_group,
    sys_task_inspector_scan_processes, sys_task_inspector_scan_threads,
};
pub(super) use ipc::{
    sys_byte_channel_create, sys_byte_channel_read, sys_byte_channel_trace_rpc,
    sys_byte_channel_write, sys_capability_channel_create, sys_capability_channel_receive,
    sys_capability_channel_try_send,
};
pub(super) use memory::{
    sys_vmar_allocate, sys_vmar_destroy, sys_vmar_lock_memory, sys_vmar_map, sys_vmar_map_file,
    sys_vmar_map_private, sys_vmar_protect, sys_vmar_unlock_memory, sys_vmar_unmap, sys_vmo_create,
    sys_vmo_create_contiguous, sys_vmo_create_snapshot, sys_vmo_read, sys_vmo_write,
};
pub(super) use object::{
    sys_event_create, sys_event_signal, sys_object_wait_many, sys_object_wait_one,
    sys_wait_set_add, sys_wait_set_create, sys_wait_set_rearm, sys_wait_set_remove,
    sys_wait_set_wait,
};
pub(super) use process_builder::{
    sys_process_builder_abort, sys_process_builder_add_handle, sys_process_builder_create,
    sys_process_builder_seal, sys_process_builder_set_affinity, sys_process_builder_set_data,
    sys_process_builder_set_memory_lock, sys_process_builder_set_name, sys_process_builder_start,
};
pub(super) use system::{
    sys_abi_query, sys_clock_get_monotonic, sys_clock_get_realtime, sys_system_config,
};
pub(super) use task::{
    sys_atomic_wait, sys_atomic_wake, sys_process_derive_observer, sys_process_exit,
    sys_process_get_current_id, sys_process_get_info, sys_process_request_stop,
    sys_resource_domain_create, sys_task_group_create, sys_task_group_request_stop,
    sys_thread_create, sys_thread_exit, sys_thread_request_stop, sys_thread_set_current_name,
    sys_thread_sleep, sys_thread_start, sys_thread_yield,
};
pub(super) use vfs::{
    sys_directory_begin_read, sys_directory_canonicalize, sys_directory_create_directory,
    sys_directory_create_file, sys_directory_get_info, sys_directory_get_metadata,
    sys_directory_get_self_metadata, sys_directory_link, sys_directory_open_directory,
    sys_directory_open_directory_nofollow, sys_directory_open_file,
    sys_directory_open_file_with_options, sys_directory_read, sys_directory_read_link,
    sys_directory_remove, sys_directory_remove_if, sys_directory_rename,
    sys_directory_scope_create, sys_directory_set_metadata, sys_directory_symlink,
    sys_file_get_info, sys_file_get_metadata, sys_file_lock, sys_file_protect, sys_file_read_at,
    sys_file_resize, sys_file_set_metadata, sys_file_sync, sys_file_unlock, sys_file_write_at,
    sys_filesystem_begin_root_switch, sys_filesystem_finish_root_switch,
    sys_filesystem_mount_ramfs, sys_filesystem_switch_root,
};
pub(super) use vm::{
    sys_guest_memory_create, sys_pending_virtual_machine_abort,
    sys_pending_virtual_machine_install, sys_pending_virtual_machine_map_memory,
    sys_pending_virtual_machine_seal, sys_pending_virtual_machine_set_bootstrap,
    sys_pending_virtual_machine_set_memory, sys_pending_virtual_machine_set_virtual_serial,
    sys_virtual_cpu_complete_mmio, sys_virtual_cpu_get_info, sys_virtual_cpu_get_mmio_request,
    sys_virtual_cpu_set_affinity, sys_virtual_cpu_start,
    sys_virtual_machine_complete_power_request, sys_virtual_machine_create,
    sys_virtual_machine_creation_lease_create,
    sys_virtual_machine_creation_lease_get_platform_info, sys_virtual_machine_get_info,
    sys_virtual_machine_get_power_request, sys_virtual_machine_open_vcpu,
    sys_virtual_machine_register_mmio, sys_virtual_machine_request_stop,
    sys_virtual_serial_acknowledge_output, sys_virtual_serial_create,
    sys_virtual_serial_register_output, sys_virtual_serial_write,
};

#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
pub(super) use ipc::capability_receive_result;
#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
pub(super) use vfs::run_wire_self_test;

mod profile;
pub(super) use profile::{
    sys_cpu_inspector_profile_read, sys_cpu_inspector_profile_start, sys_cpu_inspector_profile_stop,
};

mod trace;
pub(super) use trace::{
    sys_cpu_inspector_trace_clear, sys_cpu_inspector_trace_read, sys_cpu_inspector_trace_start,
    sys_cpu_inspector_trace_stop,
};
