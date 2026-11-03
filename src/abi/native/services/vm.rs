// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Guest I/O, VM installation, and execution service contracts.

use super::memory::UserMemoryServices;
use crate::capability::{HandleValue, InvocationReceiver};
use crate::mm::user_space::UserSlice;

pub(crate) trait GuestIoServices: UserMemoryServices {
    fn transfer_guest_memory(
        &self,
        _memory: &InvocationReceiver,
        _write: bool,
        _offset: u64,
        _buffer: UserSlice,
    ) -> Result<(), crate::vm::service::Error> {
        Err(crate::vm::service::Error::NotSupported)
    }
    fn scalar_guest_memory(
        &self,
        _memory: &InvocationReceiver,
        _write: bool,
        _offset: u64,
        _width: u32,
        _value: u64,
    ) -> Result<u64, crate::vm::service::Error> {
        Err(crate::vm::service::Error::NotSupported)
    }
    fn create_guest_interrupt(
        &self,
        _machine: &InvocationReceiver,
        _irq: u32,
    ) -> Result<HandleValue, crate::vm::service::Error> {
        Err(crate::vm::service::Error::NotSupported)
    }
    fn set_guest_interrupt_level(
        &self,
        _interrupt: &InvocationReceiver,
        _asserted: bool,
    ) -> Result<(), crate::vm::service::Error> {
        Err(crate::vm::service::Error::NotSupported)
    }

    fn create_guest_mapping(
        &self,
        _backend: &InvocationReceiver,
        _memory: HandleValue,
        _frontend: u64,
    ) -> Result<(HandleValue, u64), crate::vm::service::Error> {
        Err(crate::vm::service::Error::NotSupported)
    }
    fn release_guest_mapping(
        &self,
        _mapping: &InvocationReceiver,
    ) -> Result<(), crate::vm::service::Error> {
        Err(crate::vm::service::Error::NotSupported)
    }
    fn create_native_block(
        &self,
        _memory: &InvocationReceiver,
        _backend: HandleValue,
        _guest_base: u64,
        _notification_base: u64,
        _notification_irq: u32,
    ) -> Result<HandleValue, crate::vm::service::Error> {
        Err(crate::vm::service::Error::NotSupported)
    }
    fn activate_native_block(
        &self,
        _block: &InvocationReceiver,
        _readonly: bool,
    ) -> Result<(u64, bool), crate::block::service::ActivationError> {
        Err(crate::block::service::ActivationError::Device(
            crate::fs::primitives::block::Error::Unsupported,
        ))
    }
    fn transfer_native_block(
        &self,
        _block: &InvocationReceiver,
        _operation: u32,
        _first: u64,
        _buffer: Option<UserSlice>,
    ) -> Result<(), crate::block::service::TransferError> {
        Err(crate::block::service::TransferError::Status(
            hyper::abi::native::HYPER_NATIVE_STATUS_NOT_SUPPORTED,
        ))
    }
    fn create_block_queue(
        &self,
        _receiver: &InvocationReceiver,
        _capacity: u32,
        _byte_limit: u64,
        _inflight_limit: u32,
    ) -> Result<HandleValue, crate::block::service::TransferError> {
        Err(crate::block::service::TransferError::Status(
            hyper::abi::native::HYPER_NATIVE_STATUS_NOT_SUPPORTED,
        ))
    }
    fn submit_block_queue(
        &self,
        _receiver: &InvocationReceiver,
        _operation: u32,
        _count: u32,
        _frame: Option<UserSlice>,
    ) -> Result<u64, crate::block::service::TransferError> {
        Err(crate::block::service::TransferError::Status(
            hyper::abi::native::HYPER_NATIVE_STATUS_NOT_SUPPORTED,
        ))
    }
    fn create_block_queue_window(
        &self,
        _receiver: &InvocationReceiver,
    ) -> Result<HandleValue, crate::block::service::TransferError> {
        Err(crate::block::service::TransferError::Status(
            hyper::abi::native::HYPER_NATIVE_STATUS_NOT_SUPPORTED,
        ))
    }
    fn submit_block_queue_window(
        &self,
        _receiver: &InvocationReceiver,
        _token: u64,
        _count: u32,
        _frame: UserSlice,
    ) -> Result<u64, crate::block::service::TransferError> {
        Err(crate::block::service::TransferError::Status(
            hyper::abi::native::HYPER_NATIVE_STATUS_NOT_SUPPORTED,
        ))
    }
    fn wait_block_queue(
        &self,
        _receiver: &InvocationReceiver,
        _ticket: u64,
        _deadline: u64,
    ) -> Result<(u64, i64), crate::block::service::TransferError> {
        Err(crate::block::service::TransferError::Status(
            hyper::abi::native::HYPER_NATIVE_STATUS_NOT_SUPPORTED,
        ))
    }
    fn wait_block_queue_space(
        &self,
        _receiver: &InvocationReceiver,
        _bytes: u64,
        _deadline: u64,
    ) -> Result<(), crate::block::service::TransferError> {
        Err(crate::block::service::TransferError::Status(
            hyper::abi::native::HYPER_NATIVE_STATUS_NOT_SUPPORTED,
        ))
    }
    fn reap_block_queue(
        &self,
        _receiver: &InvocationReceiver,
        _ticket: u64,
        _output: Option<UserSlice>,
    ) -> Result<(i64, usize), crate::block::service::TransferError> {
        Err(crate::block::service::TransferError::Status(
            hyper::abi::native::HYPER_NATIVE_STATUS_NOT_SUPPORTED,
        ))
    }
    fn block_queue_info(
        &self,
        _receiver: &InvocationReceiver,
    ) -> Result<[u8; 128], crate::block::service::TransferError> {
        Err(crate::block::service::TransferError::Status(
            hyper::abi::native::HYPER_NATIVE_STATUS_NOT_SUPPORTED,
        ))
    }
    fn mount_filesystem(
        &self,
        _channel: HandleValue,
        _buffer: HandleValue,
        _queue: Option<HandleValue>,
        _directory: &InvocationReceiver,
        _path: UserSlice,
        _read_only: bool,
    ) -> Result<(), crate::fs::vfs::VfsServiceError> {
        Err(crate::fs::vfs::VfsServiceError::InvalidInput)
    }

    fn unmount_filesystem(
        &self,
        _directory: &InvocationReceiver,
        _path: UserSlice,
    ) -> Result<(), crate::fs::vfs::VfsServiceError> {
        Err(crate::fs::vfs::VfsServiceError::InvalidInput)
    }

    fn create_guest_mailbox(
        &self,
        machine: &InvocationReceiver,
        base: u64,
        irq: u32,
    ) -> Result<HandleValue, crate::vm::service::Error>;
    fn send_guest_mailbox(
        &self,
        mailbox: &InvocationReceiver,
        bytes: &[u8],
    ) -> Result<(), crate::vm::service::Error>;
    fn receive_guest_mailbox(
        &self,
        mailbox: &InvocationReceiver,
        copy: &mut dyn FnMut(&[u8]) -> Result<(), crate::vm::service::Error>,
    ) -> Result<usize, crate::vm::service::Error>;
    fn create_guest_notification(
        &self,
        frontend: &InvocationReceiver,
        backend: HandleValue,
        frontend_base: u64,
        backend_base: u64,
        frontend_irq: u32,
        backend_irq: u32,
    ) -> Result<HandleValue, crate::vm::service::Error>;
    fn access_guest_notification(
        &self,
        machine: &InvocationReceiver,
        original: u64,
        operation: u32,
        value: u32,
    ) -> Result<u32, crate::vm::service::Error>;
    fn configure_guest_notification(
        &self,
        machine: &InvocationReceiver,
        original: u64,
        base: u64,
        flags: u32,
    ) -> Result<(), crate::vm::service::Error>;
    fn control_guest_notification(
        &self,
        notification: &InvocationReceiver,
        operation: u32,
    ) -> Result<u32, crate::vm::service::Error>;
}

pub(crate) trait VmServices: UserMemoryServices {
    fn virtual_machine_platform_info(
        &self,
        lease: &InvocationReceiver,
        profile: u32,
    ) -> Result<crate::vm::service::VirtualMachinePlatformInfo, crate::vm::service::Error>;
    fn derive_virtual_machine_creation_lease(
        &self,
        authority: &InvocationReceiver,
        domain: HandleValue,
    ) -> Result<HandleValue, crate::vm::service::Error>;
    fn create_pending_virtual_machine(
        &self,
        lease: &InvocationReceiver,
        configuration: crate::vm::objects::VirtualMachineConfiguration,
    ) -> Result<HandleValue, crate::vm::service::Error>;
    fn set_pending_virtual_machine_memory(
        &self,
        pending: &InvocationReceiver,
        vmo: HandleValue,
    ) -> Result<(), crate::vm::service::Error>;
    fn set_pending_virtual_machine_bootstrap(
        &self,
        pending: &InvocationReceiver,
        bootstrap: crate::vm::objects::VirtualCpuBootstrap,
    ) -> Result<(), crate::vm::service::Error>;
    fn set_pending_virtual_machine_virtual_serial(
        &self,
        pending: &InvocationReceiver,
        console: HandleValue,
    ) -> Result<(), crate::vm::service::Error>;
    fn create_virtual_serial(&self) -> Result<HandleValue, crate::vm::service::Error>;
    fn register_virtual_serial_output(
        &self,
        serial: &InvocationReceiver,
        buffer: HandleValue,
    ) -> Result<(), crate::vm::service::Error>;
    fn acknowledge_virtual_serial_output(
        &self,
        serial: &InvocationReceiver,
        consumed: u64,
    ) -> Result<(), crate::vm::service::Error>;
    fn write_virtual_serial(
        &self,
        serial: &InvocationReceiver,
        bytes: Option<UserSlice>,
    ) -> Result<usize, crate::vm::service::Error>;
    fn seal_pending_virtual_machine(
        &self,
        pending: &InvocationReceiver,
    ) -> Result<(), crate::vm::service::Error>;
    fn install_pending_virtual_machine(
        &self,
        pending: &InvocationReceiver,
    ) -> Result<[HandleValue; 2], crate::vm::service::Error>;
    fn abort_pending_virtual_machine(
        &self,
        pending: &InvocationReceiver,
    ) -> Result<(), crate::vm::service::Error>;
    fn request_virtual_machine_stop(
        &self,
        machine: &InvocationReceiver,
    ) -> Result<(), crate::vm::service::Error>;
    fn register_mmio(
        &self,
        machine: &InvocationReceiver,
        base: u64,
        length: u64,
        device: u64,
    ) -> Result<(), crate::vm::service::Error>;
    fn pending_mmio(
        &self,
        vcpu: &InvocationReceiver,
    ) -> Result<Option<hyper_vm_models::device::mmio::Request>, crate::vm::service::Error>;
    fn complete_mmio(
        &self,
        vcpu: &InvocationReceiver,
        id: u64,
        action: hyper_vm_models::exit::MmioAction,
    ) -> Result<(), crate::vm::service::Error>;
    fn create_guest_memory(
        &self,
        vmo: &InvocationReceiver,
    ) -> Result<HandleValue, crate::vm::service::Error>;
    fn map_guest_memory(
        &self,
        pending: &InvocationReceiver,
        memory: HandleValue,
        guest_offset: u64,
        source_offset: u64,
        length: u64,
    ) -> Result<(), crate::vm::service::Error>;
    fn pending_power_request(
        &self,
        machine: &InvocationReceiver,
    ) -> Result<Option<hyper_vm_models::arm::psci::Request>, crate::vm::service::Error>;
    fn complete_power_request(
        &self,
        machine: &InvocationReceiver,
        request_id: u64,
        accept: bool,
    ) -> Result<(), crate::vm::service::Error>;
    fn open_vcpu(
        &self,
        machine: &InvocationReceiver,
        vcpu: u32,
    ) -> Result<HandleValue, crate::vm::service::Error>;
    fn virtual_machine_info(
        &self,
        machine: &InvocationReceiver,
    ) -> Result<
        (
            crate::vm::objects::VirtualMachineConfiguration,
            crate::vm::objects::VirtualMachineSnapshot,
        ),
        crate::vm::service::Error,
    >;
    fn virtual_cpu_info(
        &self,
        vcpu: &InvocationReceiver,
    ) -> Result<crate::vm::objects::VirtualCpuSnapshot, crate::vm::service::Error>;
    fn set_virtual_cpu_affinity(
        &self,
        vcpu: &InvocationReceiver,
        words: Option<UserSlice>,
        word_count: usize,
    ) -> Result<(), crate::vm::service::Error>;
    fn start_virtual_cpu(&self, vcpu: &InvocationReceiver)
    -> Result<(), crate::vm::service::Error>;
}
