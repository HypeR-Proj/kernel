// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Boot-time Native ABI contract self-test.

use crate::capability::handle_wire;

use hyper::abi::native::{
    HYPER_NATIVE_ABI_REVISION, HYPER_NATIVE_BYTE_CHANNEL_MAX_MESSAGE_BYTES,
    HYPER_NATIVE_CAPABILITY_CHANNEL_MAX_HANDLES, HYPER_NATIVE_CAPABILITY_CHANNEL_MAX_MESSAGE_BYTES,
    HYPER_NATIVE_CAPABILITY_DISPOSITION_MOVE, HYPER_NATIVE_CAPABILITY_DISPOSITION_SAME_RIGHTS,
    HYPER_NATIVE_CONSOLE_MAX_TRANSFER_BYTES, HYPER_NATIVE_DIRECTORY_ENTRY_PAGE_CAPACITY,
    HYPER_NATIVE_EXTENSIBLE_RECORD_MAX_BYTES, HYPER_NATIVE_FEATURE_CORE,
    HYPER_NATIVE_HANDLE_INFO_MIN_SIZE, HYPER_NATIVE_OBJECT_EVENT,
    HYPER_NATIVE_OBJECT_WAIT_MANY_MAX_ITEMS, HYPER_NATIVE_PROCESS_AFFINITY_MAX_WORDS,
    HYPER_NATIVE_PROCESS_NAME_MAX_BYTES, HYPER_NATIVE_STATUS_BAD_HANDLE,
    HYPER_NATIVE_STATUS_BAD_STATE, HYPER_NATIVE_STATUS_BUFFER_TOO_SMALL, HYPER_NATIVE_STATUS_BUSY,
    HYPER_NATIVE_STATUS_CANCELLED, HYPER_NATIVE_STATUS_FAULT, HYPER_NATIVE_STATUS_INVALID_ARGUMENT,
    HYPER_NATIVE_STATUS_NO_MEMORY, HYPER_NATIVE_STATUS_NOT_SUPPORTED, HYPER_NATIVE_STATUS_OK,
    HYPER_NATIVE_STATUS_PEER_CLOSED, HYPER_NATIVE_STATUS_RESOURCE_LIMIT,
    HYPER_NATIVE_STATUS_WOULD_BLOCK, HyperNativeCapabilityDisposition,
    HyperNativeCapabilityReceiveSlot, HyperNativeDirectoryInfo, HyperNativeFileInfo,
    HyperNativeObjectInspection, HyperNativeResourceLimits, NativeOperation, NativeResult,
};

use crate::task::TimedWaitError;
use {
    crate::capability::HandleFlags, crate::capability::HandleInfo, crate::capability::HandleTable,
    crate::capability::HandleTableStoragePlan, crate::capability::HandleValue,
    crate::capability::InvocationReceiver, crate::capability::PreparedHandle,
    crate::capability::Rights,
};
use {
    crate::diagnostics::inspect::HANDLE_PAGE_CAPACITY,
    crate::diagnostics::inspect::OBJECT_PAGE_CAPACITY, crate::diagnostics::inspect::Page,
    crate::diagnostics::inspect::ProcessHandleSnapshot,
    crate::diagnostics::inspect::TaskThreadSnapshot,
};
use {
    crate::fs::vfs::DirectoryInfo, crate::fs::vfs::DirectoryPage, crate::fs::vfs::FileInfo,
    crate::fs::vfs::VfsServiceError,
};
use {
    crate::ipc::ByteChannelError, crate::ipc::ByteChannelReadOutcome,
    crate::ipc::ByteChannelServiceError, crate::ipc::CapabilityChannelError,
    crate::ipc::CapabilityChannelServiceError, crate::ipc::CapabilityReceiveOutcome,
};
use {
    crate::mm::user_space::MemoryServiceError, crate::mm::user_space::Permissions,
    crate::mm::user_space::UserSlice,
};
use {
    crate::object::Event, crate::object::ObjectWaitError, crate::object::PublishableRef,
    crate::object::SignalWaitManyOutcome, crate::object::SignalWaitOutcome,
};
use {crate::process::ProcessError, crate::process::ProcessSnapshot};

use super::dispatch::dispatch_operation;
use super::handlers::capability_receive_result;
use super::services::{
    ConsoleServiceError, ConsoleServices, HandleServices, HierarchyServices, InspectServices,
    IpcServices, MemoryServices, NativeAction, ObjectServiceError, ObjectServices,
    ProcessBuilderServiceError, ProcessBuilderServices, SystemInspectServices, TaskServices,
    UserMemoryServices, VfsServices, VmServices,
};
use super::status::{
    console_io_result, failure, status_from_byte_channel_error,
    status_from_capability_channel_error, status_from_console_service_error,
    status_from_object_wait_error, status_from_vm_service_error, success,
};
use super::wire::{
    HANDLE_INFO_SIZE, PROCESS_INFO_SIZE, capability_disposition_bytes,
    capability_receive_slot_bytes, capability_record_bytes, copy_extensible_input_record,
    copy_info_record, decode_resource_limits, encode_directory_info_fields,
    encode_file_info_fields, encode_handle_info_fields, encode_object_basic_info_fields,
    encode_object_inspection, encode_process_info_fields, parse_builder_create,
    parse_builder_handle, prepare_info_request,
};

#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SelfTestError {
    Admission,
    AbiQuery,
    Clock,
    InvalidRights,
    InvalidRecordSize,
    ChannelValidation,
    CapabilityChannelValidation,
    CapabilityChannelErrorMapping,
    ConsoleValidation,
    ConsoleErrorMapping,
    ProcessBuilderValidation,
    ObjectErrorMapping,
    VmErrorMapping,
    ChannelErrorMapping,
    RecordEncoding,
    ValidationReachedService,
    DispatchAction,
}

#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
pub(crate) fn run_self_test() -> Result<(), SelfTestError> {
    super::admission_test::run().map_err(|_| SelfTestError::Admission)?;
    if super::wire::parse_object_kind(hyper::abi::native::HYPER_NATIVE_OBJECT_NATIVE_BLOCK)
        != Ok(crate::object::ObjectKind::NATIVE_BLOCK)
    {
        return Err(SelfTestError::ProcessBuilderValidation);
    }
    use core::cell::Cell;

    struct RejectingServices {
        calls: Cell<usize>,
    }

    struct RecordInput {
        nonzero_at: Option<u64>,
    }

    struct RecordOutput {
        written: Cell<usize>,
    }

    impl UserMemoryServices for RecordInput {
        fn copy_to_user(&self, _: UserSlice, _: &[u8]) -> Result<(), ProcessError> {
            Err(ProcessError::Allocation)
        }

        fn copy_from_user(
            &self,
            source: UserSlice,
            destination: &mut [u8],
        ) -> Result<(), ProcessError> {
            destination.fill(0);
            if let Some(nonzero_at) = self.nonzero_at {
                let base = source.base().get();
                let end = source.end().get();
                if nonzero_at >= base && nonzero_at < end {
                    destination[(nonzero_at - base) as usize] = 1;
                }
            }
            Ok(())
        }
    }

    impl UserMemoryServices for RecordOutput {
        fn copy_to_user(&self, _: UserSlice, source: &[u8]) -> Result<(), ProcessError> {
            self.written.set(source.len());
            Ok(())
        }

        fn copy_from_user(&self, _: UserSlice, _: &mut [u8]) -> Result<(), ProcessError> {
            Err(ProcessError::Allocation)
        }
    }

    impl RejectingServices {
        fn reached(&self) -> Result<(), SelfTestError> {
            if self.calls.get() == 0 {
                Ok(())
            } else {
                Err(SelfTestError::ValidationReachedService)
            }
        }
    }

    impl UserMemoryServices for RejectingServices {
        fn copy_to_user(&self, _: UserSlice, _: &[u8]) -> Result<(), ProcessError> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(ProcessError::Allocation)
        }

        fn copy_from_user(&self, _: UserSlice, _: &mut [u8]) -> Result<(), ProcessError> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(ProcessError::Allocation)
        }
    }

    impl HandleServices for RejectingServices {
        fn resolve_invocation(&self, _: HandleValue) -> Result<InvocationReceiver, ProcessError> {
            Err(ProcessError::Allocation)
        }
        fn validate_invocation_context(&self, _: &InvocationReceiver) -> Result<(), ProcessError> {
            Err(ProcessError::Allocation)
        }

        fn close_handle(&self, _: &InvocationReceiver) -> Result<(), ProcessError> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(ProcessError::Allocation)
        }

        fn handle_info(
            &self,
            _: &InvocationReceiver,
            _: Rights,
        ) -> Result<HandleInfo, ProcessError> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(ProcessError::Allocation)
        }

        fn duplicate_handle(
            &self,
            _: &InvocationReceiver,
            _: Rights,
        ) -> Result<HandleValue, ProcessError> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(ProcessError::Allocation)
        }
        fn replace_handle(
            &self,
            _: &InvocationReceiver,
            _: Rights,
        ) -> Result<HandleValue, ProcessError> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(ProcessError::Allocation)
        }
    }

    impl ObjectServices for RejectingServices {
        fn current_process_id(&self) -> u64 {
            1
        }
        fn wait_set_create(&self, _capacity: usize) -> Result<HandleValue, ObjectServiceError> {
            Err(ObjectServiceError::InvalidInput)
        }

        fn wait_set_add(
            &self,
            _set: &InvocationReceiver,
            _source: HandleValue,
            _signals: u64,
        ) -> Result<u64, ObjectServiceError> {
            Err(ObjectServiceError::InvalidInput)
        }

        fn wait_set_rearm(
            &self,
            _set: &InvocationReceiver,
            _registration: u64,
        ) -> Result<(), ObjectServiceError> {
            Err(ObjectServiceError::InvalidInput)
        }

        fn wait_set_remove(
            &self,
            _set: &InvocationReceiver,
            _registration: u64,
        ) -> Result<(), ObjectServiceError> {
            Err(ObjectServiceError::InvalidInput)
        }

        fn wait_set_wait(
            &self,
            _set: &InvocationReceiver,
            _deadline: u64,
            _output: UserSlice,
        ) -> Result<(), ObjectServiceError> {
            Err(ObjectServiceError::InvalidInput)
        }

        fn create_event(&self) -> Result<HandleValue, ObjectServiceError> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(ProcessError::Allocation.into())
        }

        fn signal_event(
            &self,
            _: &InvocationReceiver,
            _: u64,
            _: u64,
        ) -> Result<(), ObjectServiceError> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(ProcessError::Allocation.into())
        }

        fn wait_one(
            &self,
            _: &InvocationReceiver,
            _: u64,
            _: u64,
        ) -> Result<SignalWaitOutcome, ObjectServiceError> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(ProcessError::Allocation.into())
        }

        fn wait_many(
            &self,
            _: UserSlice,
            _: usize,
            _: u64,
        ) -> Result<SignalWaitManyOutcome, ObjectServiceError> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(ProcessError::Allocation.into())
        }
    }

    impl IpcServices for RejectingServices {
        fn create_byte_channel(&self) -> Result<[HandleValue; 2], ByteChannelServiceError> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(ByteChannelServiceError::Process(ProcessError::Allocation))
        }
        fn create_capability_channel(
            &self,
        ) -> Result<[HandleValue; 2], CapabilityChannelServiceError> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(CapabilityChannelServiceError::Process(
                ProcessError::Allocation,
            ))
        }

        fn trace_byte_channel_rpc(
            &self,
            _: &InvocationReceiver,
            _: &[u64; 6],
        ) -> Result<[u64; 2], hyper::abi::native::HyperNativeStatus> {
            Err(HYPER_NATIVE_STATUS_NOT_SUPPORTED)
        }

        fn write_byte_channel(
            &self,
            _: &InvocationReceiver,
            _: Option<UserSlice>,
        ) -> Result<(), ByteChannelServiceError> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(ByteChannelServiceError::Process(ProcessError::Allocation))
        }

        fn read_byte_channel(
            &self,
            _: &InvocationReceiver,
            _: Option<UserSlice>,
        ) -> Result<ByteChannelReadOutcome, ByteChannelServiceError> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(ByteChannelServiceError::Process(ProcessError::Allocation))
        }

        fn try_send_capability_channel(
            &self,
            _: &InvocationReceiver,
            _: Option<UserSlice>,
            _: Option<UserSlice>,
        ) -> Result<(), CapabilityChannelServiceError> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(CapabilityChannelServiceError::Process(
                ProcessError::Allocation,
            ))
        }

        fn receive_capability_channel(
            &self,
            _: &InvocationReceiver,
            _: u64,
            _: Option<UserSlice>,
            _: Option<UserSlice>,
        ) -> Result<CapabilityReceiveOutcome, CapabilityChannelServiceError> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(CapabilityChannelServiceError::Process(
                ProcessError::Allocation,
            ))
        }
    }

    impl ConsoleServices for RejectingServices {
        fn discard_console_input(
            &self,
            _: &InvocationReceiver,
        ) -> Result<usize, ConsoleServiceError> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(ConsoleServiceError::Process(ProcessError::Allocation))
        }

        fn read_console(
            &self,
            _: &InvocationReceiver,
            _: Option<UserSlice>,
        ) -> Result<usize, ConsoleServiceError> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(ConsoleServiceError::Process(ProcessError::Allocation))
        }

        fn write_console(
            &self,
            _: &InvocationReceiver,
            _: Option<UserSlice>,
        ) -> Result<usize, ConsoleServiceError> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(ConsoleServiceError::Process(ProcessError::Allocation))
        }
    }

    impl VfsServices for RejectingServices {
        fn mount_ramfs(
            &self,
            _directory: &InvocationReceiver,
            _path: UserSlice,
        ) -> Result<(), VfsServiceError> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(VfsServiceError::InvalidInput)
        }
        fn switch_root(
            &self,
            _directory: &InvocationReceiver,
            _request: UserSlice,
        ) -> Result<(), VfsServiceError> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(VfsServiceError::InvalidInput)
        }

        fn begin_root_switch(
            &self,
            _: &InvocationReceiver,
            _: UserSlice,
        ) -> Result<HandleValue, VfsServiceError> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(VfsServiceError::InvalidInput)
        }
        fn finish_root_switch(
            &self,
            _: &InvocationReceiver,
            _: bool,
        ) -> Result<(), VfsServiceError> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(VfsServiceError::InvalidInput)
        }
        fn directory_scope_create(
            &self,
            _root: &InvocationReceiver,
            _start: HandleValue,
            _rights: Rights,
        ) -> Result<HandleValue, VfsServiceError> {
            Err(VfsServiceError::InvalidInput)
        }
        fn directory_get_metadata(
            &self,
            _directory: &InvocationReceiver,
            _path: UserSlice,
            _follow: bool,
        ) -> Result<crate::fs::vfs::Metadata, VfsServiceError> {
            Err(VfsServiceError::InvalidInput)
        }
        fn file_get_metadata(
            &self,
            _file: &InvocationReceiver,
        ) -> Result<crate::fs::vfs::Metadata, VfsServiceError> {
            Err(VfsServiceError::InvalidInput)
        }
        fn directory_get_self_metadata(
            &self,
            _directory: &InvocationReceiver,
        ) -> Result<crate::fs::vfs::Metadata, VfsServiceError> {
            Err(VfsServiceError::InvalidInput)
        }
        fn directory_set_metadata(
            &self,
            _directory: &InvocationReceiver,
            _path: UserSlice,
            _follow: bool,
            _update: crate::fs::vfs::MetadataUpdate,
        ) -> Result<(), VfsServiceError> {
            Err(VfsServiceError::InvalidInput)
        }
        fn file_set_metadata(
            &self,
            _file: &InvocationReceiver,
            _update: crate::fs::vfs::MetadataUpdate,
        ) -> Result<(), VfsServiceError> {
            Err(VfsServiceError::InvalidInput)
        }
        fn directory_rename(
            &self,
            _source: &InvocationReceiver,
            _path: UserSlice,
            _destination: HandleValue,
            _new_path: UserSlice,
        ) -> Result<(), VfsServiceError> {
            Err(VfsServiceError::InvalidInput)
        }
        fn directory_link(
            &self,
            _source: &InvocationReceiver,
            _path: UserSlice,
            _destination: HandleValue,
            _new_path: UserSlice,
        ) -> Result<(), VfsServiceError> {
            Err(VfsServiceError::InvalidInput)
        }
        fn directory_symlink(
            &self,
            _directory: &InvocationReceiver,
            _path: UserSlice,
            _target: UserSlice,
        ) -> Result<(), VfsServiceError> {
            Err(VfsServiceError::InvalidInput)
        }
        fn directory_read_link(
            &self,
            _directory: &InvocationReceiver,
            _path: UserSlice,
        ) -> Result<crate::fs::vfs::ScratchVec<u8>, VfsServiceError> {
            Err(VfsServiceError::InvalidInput)
        }
        fn directory_canonicalize(
            &self,
            _directory: &InvocationReceiver,
            _path: UserSlice,
        ) -> Result<crate::fs::vfs::ScratchString, VfsServiceError> {
            Err(VfsServiceError::InvalidInput)
        }
        fn directory_remove_if(
            &self,
            _directory: &InvocationReceiver,
            _path: UserSlice,
            _is_directory: bool,
            _expected: u64,
        ) -> Result<(), VfsServiceError> {
            Err(VfsServiceError::InvalidInput)
        }
        fn directory_open_directory_nofollow(
            &self,
            _directory: &InvocationReceiver,
            _path: UserSlice,
            _rights: Rights,
        ) -> Result<HandleValue, VfsServiceError> {
            Err(VfsServiceError::InvalidInput)
        }
        fn file_sync(
            &self,
            _file: &InvocationReceiver,
            _scope: u64,
        ) -> Result<(), VfsServiceError> {
            Err(VfsServiceError::InvalidInput)
        }
        fn file_lock(
            &self,
            _file: &InvocationReceiver,
            _mode: crate::fs::vfs::locks::LockMode,
            _deadline: u64,
        ) -> Result<(), VfsServiceError> {
            Err(VfsServiceError::InvalidInput)
        }
        fn file_unlock(&self, _file: &InvocationReceiver) -> Result<(), VfsServiceError> {
            Err(VfsServiceError::InvalidInput)
        }
        fn directory_open_file_with_options(
            &self,
            _directory: &InvocationReceiver,
            _path: UserSlice,
            _rights: Rights,
            _options: u64,
            _mode: u32,
        ) -> Result<HandleValue, VfsServiceError> {
            Err(VfsServiceError::InvalidInput)
        }

        fn create_file(
            &self,
            _directory: &InvocationReceiver,
            _path: UserSlice,
            _rights: Rights,
            _mode: u32,
        ) -> Result<HandleValue, VfsServiceError> {
            Err(VfsServiceError::InvalidInput)
        }

        fn create_directory(
            &self,
            _directory: &InvocationReceiver,
            _path: UserSlice,
            _mode: u32,
        ) -> Result<(), VfsServiceError> {
            Err(VfsServiceError::InvalidInput)
        }

        fn remove_entry(
            &self,
            _directory: &InvocationReceiver,
            _path: UserSlice,
            _is_directory: bool,
        ) -> Result<(), VfsServiceError> {
            Err(VfsServiceError::InvalidInput)
        }

        fn resize_file(
            &self,
            _file: &InvocationReceiver,
            _length: u64,
        ) -> Result<(), VfsServiceError> {
            Err(VfsServiceError::InvalidInput)
        }

        fn write_file_at(
            &self,
            _file: &InvocationReceiver,
            _offset: Option<u64>,
            _input: Option<UserSlice>,
        ) -> Result<(u64, u64), VfsServiceError> {
            Err(VfsServiceError::InvalidInput)
        }

        fn open_file(
            &self,
            _: &InvocationReceiver,
            _: UserSlice,
            _: Rights,
        ) -> Result<HandleValue, VfsServiceError> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(VfsServiceError::Process(ProcessError::Allocation))
        }

        fn open_directory(
            &self,
            _: &InvocationReceiver,
            _: UserSlice,
            _: Rights,
        ) -> Result<HandleValue, VfsServiceError> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(VfsServiceError::Process(ProcessError::Allocation))
        }

        fn begin_directory_read(
            &self,
            _: &InvocationReceiver,
        ) -> Result<HandleValue, VfsServiceError> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(VfsServiceError::Process(ProcessError::Allocation))
        }

        fn read_directory(
            &self,
            _: &InvocationReceiver,
            _: u64,
            _: &mut DirectoryPage,
        ) -> Result<(), VfsServiceError> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(VfsServiceError::Process(ProcessError::Allocation))
        }

        fn read_file_at(
            &self,
            _: &InvocationReceiver,
            _: u64,
            _: Option<UserSlice>,
        ) -> Result<(u64, u64), VfsServiceError> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(VfsServiceError::Process(ProcessError::Allocation))
        }

        fn protect_file(
            &self,
            _: &InvocationReceiver,
        ) -> Result<(HandleValue, u64), VfsServiceError> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(VfsServiceError::FileSystem(crate::fs::vfs::VfsError::Busy))
        }
        fn file_info(&self, _: &InvocationReceiver) -> Result<FileInfo, VfsServiceError> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(VfsServiceError::Process(ProcessError::Allocation))
        }

        fn directory_info(&self, _: &InvocationReceiver) -> Result<DirectoryInfo, VfsServiceError> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(VfsServiceError::Process(ProcessError::Allocation))
        }
    }

    impl MemoryServices for RejectingServices {
        fn set_memory_residency(
            &self,
            _: &InvocationReceiver,
            _: u64,
            _: u64,
            _: bool,
            _: bool,
        ) -> Result<(), MemoryServiceError> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(MemoryServiceError::Process(ProcessError::Allocation))
        }
        fn create_contiguous_vmo(&self, _: u64) -> Result<HandleValue, MemoryServiceError> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(MemoryServiceError::Process(ProcessError::Allocation))
        }
        fn create_vmo(&self, _: u64) -> Result<HandleValue, MemoryServiceError> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(MemoryServiceError::Process(ProcessError::Allocation))
        }

        fn create_vmo_snapshot(
            &self,
            _: &InvocationReceiver,
        ) -> Result<(HandleValue, u64), MemoryServiceError> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(MemoryServiceError::Process(ProcessError::Allocation))
        }
        fn map_file(
            &self,
            _: &InvocationReceiver,
            _: HandleValue,
            _: HandleValue,
            _: crate::mm::user_space::PrivateMappingRequest,
        ) -> Result<(), MemoryServiceError> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(MemoryServiceError::Process(ProcessError::Allocation))
        }
        fn map_private(
            &self,
            _: &InvocationReceiver,
            _: HandleValue,
            _: crate::mm::user_space::PrivateMappingRequest,
        ) -> Result<(), MemoryServiceError> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(MemoryServiceError::Process(ProcessError::Allocation))
        }
        fn read_vmo(
            &self,
            _: &InvocationReceiver,
            _: u64,
            _: Option<UserSlice>,
        ) -> Result<(), MemoryServiceError> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(MemoryServiceError::Process(ProcessError::Allocation))
        }

        fn write_vmo(
            &self,
            _: &InvocationReceiver,
            _: u64,
            _: Option<UserSlice>,
        ) -> Result<(), MemoryServiceError> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(MemoryServiceError::Process(ProcessError::Allocation))
        }

        fn map_vmo(
            &self,
            _: &InvocationReceiver,
            _: HandleValue,
            _: u64,
            _: u64,
            _: u64,
            _: Permissions,
        ) -> Result<(), MemoryServiceError> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(MemoryServiceError::Process(ProcessError::Allocation))
        }

        fn allocate_vmar(
            &self,
            _: &InvocationReceiver,
            _: u64,
            _: u64,
            _: bool,
        ) -> Result<(HandleValue, u64), MemoryServiceError> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(MemoryServiceError::Process(ProcessError::Allocation))
        }

        fn protect_vmar(
            &self,
            _: &InvocationReceiver,
            _: u64,
            _: u64,
            _: Permissions,
        ) -> Result<(), MemoryServiceError> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(MemoryServiceError::Process(ProcessError::Allocation))
        }

        fn unmap_vmar(
            &self,
            _: &InvocationReceiver,
            _: u64,
            _: u64,
        ) -> Result<(), MemoryServiceError> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(MemoryServiceError::Process(ProcessError::Allocation))
        }

        fn destroy_vmar(&self, _: &InvocationReceiver) -> Result<(), MemoryServiceError> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(MemoryServiceError::Process(ProcessError::Allocation))
        }
    }

    impl crate::abi::native::RetentionServices for RejectingServices {}

    impl ProcessBuilderServices for RejectingServices {
        fn create_process_builder(
            &self,
            _: &InvocationReceiver,
            _: HandleValue,
            _: HandleValue,
            _: HandleValue,
            _: HandleValue,
        ) -> Result<HandleValue, ProcessBuilderServiceError> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(ProcessBuilderServiceError::Process(
                ProcessError::Allocation,
            ))
        }

        fn set_process_builder_name(
            &self,
            _: &InvocationReceiver,
            _: Option<UserSlice>,
        ) -> Result<(), ProcessBuilderServiceError> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(ProcessBuilderServiceError::Process(
                ProcessError::Allocation,
            ))
        }

        fn set_process_builder_data(
            &self,
            _: &InvocationReceiver,
            _: Option<UserSlice>,
        ) -> Result<(), ProcessBuilderServiceError> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(ProcessBuilderServiceError::Process(
                ProcessError::Allocation,
            ))
        }

        fn set_process_builder_affinity(
            &self,
            _: &InvocationReceiver,
            _: Option<UserSlice>,
            _: usize,
        ) -> Result<(), ProcessBuilderServiceError> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(ProcessBuilderServiceError::Process(
                ProcessError::Allocation,
            ))
        }

        fn set_process_builder_memory_lock(
            &self,
            _: &InvocationReceiver,
            _: bool,
        ) -> Result<(), ProcessBuilderServiceError> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(ProcessBuilderServiceError::Process(
                ProcessError::Allocation,
            ))
        }

        fn add_process_builder_handle(
            &self,
            _: &InvocationReceiver,
            _: HandleValue,
            _: u32,
            _: crate::object::ObjectKind,
            _: Option<Rights>,
            _: crate::capability::HandleTransferOperation,
        ) -> Result<(), ProcessBuilderServiceError> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(ProcessBuilderServiceError::Process(
                ProcessError::Allocation,
            ))
        }

        fn seal_process_builder(
            &self,
            _: &InvocationReceiver,
        ) -> Result<(), ProcessBuilderServiceError> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(ProcessBuilderServiceError::Process(
                ProcessError::Allocation,
            ))
        }

        fn start_process_builder(
            &self,
            _: &InvocationReceiver,
        ) -> Result<[HandleValue; 2], ProcessBuilderServiceError> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(ProcessBuilderServiceError::Process(
                ProcessError::Allocation,
            ))
        }

        fn abort_process_builder(
            &self,
            _: &InvocationReceiver,
        ) -> Result<(), ProcessBuilderServiceError> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(ProcessBuilderServiceError::Process(
                ProcessError::Allocation,
            ))
        }
    }

    impl TaskServices for RejectingServices {
        fn set_current_thread_name(&self, _: &str) -> Result<(), ProcessError> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(ProcessError::Allocation)
        }
        fn create_thread(
            &self,
            _scheduling_rights: Rights,
            _: u64,
            _: u64,
            _: u64,
            _: u64,
            _: Option<UserSlice>,
            _: usize,
        ) -> Result<HandleValue, ObjectServiceError> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(ObjectServiceError::InvalidInput)
        }
        fn start_thread(&self, _: &InvocationReceiver) -> Result<(), ProcessError> {
            Err(ProcessError::Allocation)
        }
        fn stop_thread(&self, _: &InvocationReceiver) -> Result<(), ProcessError> {
            Err(ProcessError::Allocation)
        }
        fn atomic_wait(
            &self,
            _: u64,
            _: u32,
            _: u64,
        ) -> Result<crate::task::WaitOutcome, ObjectServiceError> {
            Err(ObjectServiceError::InvalidInput)
        }
        fn atomic_wake(&self, _: u64, _: u32) -> Result<u64, ObjectServiceError> {
            Err(ObjectServiceError::InvalidInput)
        }
        fn sleep_thread(&self, _: u64) -> Result<crate::task::WaitOutcome, ObjectServiceError> {
            Err(ObjectServiceError::InvalidInput)
        }

        fn process_info(&self, _: &InvocationReceiver) -> Result<ProcessSnapshot, ProcessError> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(ProcessError::Allocation)
        }

        fn derive_process_observer(
            &self,
            _: &InvocationReceiver,
        ) -> Result<HandleValue, ProcessError> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(ProcessError::Allocation)
        }

        fn request_task_group_stop(&self, _: &InvocationReceiver) -> Result<(), ProcessError> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(ProcessError::Allocation)
        }

        fn request_process_stop(&self, _: &InvocationReceiver) -> Result<(), ProcessError> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(ProcessError::Allocation)
        }
    }

    impl HierarchyServices for RejectingServices {
        fn create_resource_domain(
            &self,
            _: &InvocationReceiver,
            _: crate::accounting::ResourceLimits,
        ) -> Result<HandleValue, crate::process::hierarchy::Error> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(crate::process::hierarchy::Error::NotSupported)
        }

        fn create_task_group(
            &self,
            _: &InvocationReceiver,
            _: HandleValue,
        ) -> Result<HandleValue, crate::process::hierarchy::Error> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(crate::process::hierarchy::Error::NotSupported)
        }
    }

    impl crate::abi::native::DeviceServices for RejectingServices {
        fn device_firmware_read(
            &self,
            _: &InvocationReceiver,
            _: u32,
            _: u32,
            _: &str,
        ) -> Result<alloc::vec::Vec<u8>, crate::device::assigned::service::MatchError> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(crate::device::assigned::service::MatchError::Service(
                crate::vm::service::Error::NotSupported,
            ))
        }
        fn device_claim_bundle(
            &self,
            _: &InvocationReceiver,
            _: &[(u32, u32, u64)],
            _: u32,
        ) -> Result<HandleValue, crate::vm::service::Error> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(crate::vm::service::Error::NotSupported)
        }
        fn device_mmio(
            &self,
            _: &InvocationReceiver,
            _: u64,
            _: u32,
            _: bool,
            _: u64,
        ) -> Result<u64, crate::vm::service::Error> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(crate::vm::service::Error::NotSupported)
        }
        fn device_irq_pending(
            &self,
            _: &InvocationReceiver,
        ) -> Result<u64, crate::vm::service::Error> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(crate::vm::service::Error::NotSupported)
        }
        fn device_irq_complete(
            &self,
            _: &InvocationReceiver,
            _: u64,
            _: bool,
        ) -> Result<(), crate::vm::service::Error> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(crate::vm::service::Error::NotSupported)
        }

        fn device_dma_fault_info(
            &self,
            device: &InvocationReceiver,
        ) -> Result<[u8; 40], crate::vm::service::Error> {
            let _ = device;
            Err(crate::vm::service::Error::NotSupported)
        }

        fn device_profile_info(
            &self,
            device: &InvocationReceiver,
        ) -> Result<[u8; 32], crate::vm::service::Error> {
            {
                let _ = device;
                Err(crate::vm::service::Error::NotSupported)
            }
        }
        fn device_resource_info(
            &self,
            device: &InvocationReceiver,
            index: u32,
        ) -> Result<[u8; 32], crate::vm::service::Error> {
            {
                let _ = (device, index);
                Err(crate::vm::service::Error::NotSupported)
            }
        }
        fn claim_device_matching(
            &self,
            authority: &InvocationReceiver,
            profile: u32,
            identity_kind: u32,
            identity: &str,
        ) -> Result<HandleValue, crate::device::assigned::service::MatchError> {
            {
                let _ = (authority, profile, identity_kind, identity);
                Err(crate::device::assigned::service::MatchError::Service(
                    crate::vm::service::Error::NotSupported,
                ))
            }
        }
        fn claim_device(
            &self,
            _: &InvocationReceiver,
            _: u32,
        ) -> Result<HandleValue, crate::vm::service::Error> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(crate::vm::service::Error::NotSupported)
        }
        fn physical_device_info(
            &self,
            _: &InvocationReceiver,
        ) -> Result<crate::device::assigned::Info, crate::vm::service::Error> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(crate::vm::service::Error::NotSupported)
        }
        fn vmo_dma_extent(
            &self,
            _: &InvocationReceiver,
            _: HandleValue,
            _: u64,
            _: u64,
        ) -> Result<crate::device::assigned::DmaExtent, crate::vm::service::Error> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(crate::vm::service::Error::NotSupported)
        }
        fn assign_physical_device(
            &self,
            _: &InvocationReceiver,
            _: HandleValue,
            _: u64,
            _: u32,
        ) -> Result<(), crate::vm::service::Error> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(crate::vm::service::Error::NotSupported)
        }
    }
    impl crate::abi::native::GuestIoServices for RejectingServices {
        fn create_block_queue(
            &self,
            _: &InvocationReceiver,
            _: u32,
            _: u64,
            _: u32,
        ) -> Result<HandleValue, crate::block::service::TransferError> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(crate::block::service::TransferError::Status(
                HYPER_NATIVE_STATUS_NOT_SUPPORTED,
            ))
        }
        fn submit_block_queue(
            &self,
            _: &InvocationReceiver,
            _: u32,
            _: u32,
            _: Option<UserSlice>,
        ) -> Result<u64, crate::block::service::TransferError> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(crate::block::service::TransferError::Status(
                HYPER_NATIVE_STATUS_NOT_SUPPORTED,
            ))
        }
        fn wait_block_queue(
            &self,
            _: &InvocationReceiver,
            ticket: u64,
            _: u64,
        ) -> Result<(u64, i64), crate::block::service::TransferError> {
            self.calls.set(self.calls.get().saturating_add(1));
            Ok((ticket, hyper::abi::native::HYPER_NATIVE_STATUS_IO_ERROR))
        }
        fn wait_block_queue_space(
            &self,
            _: &InvocationReceiver,
            _: u64,
            _: u64,
        ) -> Result<(), crate::block::service::TransferError> {
            self.calls.set(self.calls.get().saturating_add(1));
            Ok(())
        }
        fn reap_block_queue(
            &self,
            _: &InvocationReceiver,
            _: u64,
            _: Option<UserSlice>,
        ) -> Result<(i64, usize), crate::block::service::TransferError> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(crate::block::service::TransferError::Status(
                HYPER_NATIVE_STATUS_NOT_SUPPORTED,
            ))
        }

        fn create_guest_mailbox(
            &self,
            _: &InvocationReceiver,
            _: u64,
            _: u32,
        ) -> Result<HandleValue, crate::vm::service::Error> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(crate::vm::service::Error::NotSupported)
        }
        fn send_guest_mailbox(
            &self,
            _: &InvocationReceiver,
            _: &[u8],
        ) -> Result<(), crate::vm::service::Error> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(crate::vm::service::Error::NotSupported)
        }
        fn receive_guest_mailbox(
            &self,
            _: &InvocationReceiver,
            _: &mut dyn FnMut(&[u8]) -> Result<(), crate::vm::service::Error>,
        ) -> Result<usize, crate::vm::service::Error> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(crate::vm::service::Error::NotSupported)
        }
        fn create_guest_notification(
            &self,
            _: &InvocationReceiver,
            _: HandleValue,
            _: u64,
            _: u64,
            _: u32,
            _: u32,
        ) -> Result<HandleValue, crate::vm::service::Error> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(crate::vm::service::Error::NotSupported)
        }
        fn access_guest_notification(
            &self,
            _: &InvocationReceiver,
            _: u64,
            _: u32,
            _: u32,
        ) -> Result<u32, crate::vm::service::Error> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(crate::vm::service::Error::NotSupported)
        }
        fn configure_guest_notification(
            &self,
            _: &InvocationReceiver,
            _: u64,
            _: u64,
            _: u32,
        ) -> Result<(), crate::vm::service::Error> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(crate::vm::service::Error::NotSupported)
        }
        fn control_guest_notification(
            &self,
            _: &InvocationReceiver,
            _: u32,
        ) -> Result<u32, crate::vm::service::Error> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(crate::vm::service::Error::NotSupported)
        }
    }
    impl VmServices for RejectingServices {
        fn virtual_machine_platform_info(
            &self,
            _: &InvocationReceiver,
            _: u32,
        ) -> Result<crate::vm::service::VirtualMachinePlatformInfo, crate::vm::service::Error>
        {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(crate::vm::service::Error::NotSupported)
        }

        fn derive_virtual_machine_creation_lease(
            &self,
            _: &InvocationReceiver,
            _: HandleValue,
        ) -> Result<HandleValue, crate::vm::service::Error> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(crate::vm::service::Error::NotSupported)
        }

        fn create_pending_virtual_machine(
            &self,
            _: &InvocationReceiver,
            _: crate::vm::objects::VirtualMachineConfiguration,
        ) -> Result<HandleValue, crate::vm::service::Error> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(crate::vm::service::Error::NotSupported)
        }

        fn set_pending_virtual_machine_memory(
            &self,
            _: &InvocationReceiver,
            _: HandleValue,
        ) -> Result<(), crate::vm::service::Error> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(crate::vm::service::Error::NotSupported)
        }

        fn set_pending_virtual_machine_bootstrap(
            &self,
            _: &InvocationReceiver,
            _: crate::vm::objects::VirtualCpuBootstrap,
        ) -> Result<(), crate::vm::service::Error> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(crate::vm::service::Error::NotSupported)
        }

        fn set_pending_virtual_machine_virtual_serial(
            &self,
            _: &InvocationReceiver,
            _: HandleValue,
        ) -> Result<(), crate::vm::service::Error> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(crate::vm::service::Error::NotSupported)
        }

        fn create_virtual_serial(&self) -> Result<HandleValue, crate::vm::service::Error> {
            Err(crate::vm::service::Error::NotSupported)
        }

        fn register_virtual_serial_output(
            &self,
            _: &InvocationReceiver,
            _: HandleValue,
        ) -> Result<(), crate::vm::service::Error> {
            Err(crate::vm::service::Error::NotSupported)
        }
        fn acknowledge_virtual_serial_output(
            &self,
            _: &InvocationReceiver,
            _: u64,
        ) -> Result<(), crate::vm::service::Error> {
            Err(crate::vm::service::Error::NotSupported)
        }
        fn write_virtual_serial(
            &self,
            _: &InvocationReceiver,
            _: Option<UserSlice>,
        ) -> Result<usize, crate::vm::service::Error> {
            Err(crate::vm::service::Error::NotSupported)
        }

        fn seal_pending_virtual_machine(
            &self,
            _: &InvocationReceiver,
        ) -> Result<(), crate::vm::service::Error> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(crate::vm::service::Error::NotSupported)
        }

        fn install_pending_virtual_machine(
            &self,
            _: &InvocationReceiver,
        ) -> Result<[HandleValue; 2], crate::vm::service::Error> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(crate::vm::service::Error::NotSupported)
        }

        fn abort_pending_virtual_machine(
            &self,
            _: &InvocationReceiver,
        ) -> Result<(), crate::vm::service::Error> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(crate::vm::service::Error::NotSupported)
        }

        fn request_virtual_machine_stop(
            &self,
            _: &InvocationReceiver,
        ) -> Result<(), crate::vm::service::Error> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(crate::vm::service::Error::NotSupported)
        }

        fn register_mmio(
            &self,
            _: &InvocationReceiver,
            _: u64,
            _: u64,
            _: u64,
        ) -> Result<(), crate::vm::service::Error> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(crate::vm::service::Error::NotSupported)
        }
        fn pending_mmio(
            &self,
            _: &InvocationReceiver,
        ) -> Result<Option<hyper_vm_models::device::mmio::Request>, crate::vm::service::Error>
        {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(crate::vm::service::Error::NotSupported)
        }
        fn complete_mmio(
            &self,
            _: &InvocationReceiver,
            _: u64,
            _: hyper_vm_models::exit::MmioAction,
        ) -> Result<(), crate::vm::service::Error> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(crate::vm::service::Error::NotSupported)
        }
        fn create_guest_memory(
            &self,
            _: &InvocationReceiver,
        ) -> Result<HandleValue, crate::vm::service::Error> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(crate::vm::service::Error::NotSupported)
        }
        fn map_guest_memory(
            &self,
            _: &InvocationReceiver,
            _: HandleValue,
            _: u64,
            _: u64,
            _: u64,
        ) -> Result<(), crate::vm::service::Error> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(crate::vm::service::Error::NotSupported)
        }
        fn pending_power_request(
            &self,
            _: &InvocationReceiver,
        ) -> Result<Option<hyper_vm_models::arm::psci::Request>, crate::vm::service::Error>
        {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(crate::vm::service::Error::NotSupported)
        }
        fn complete_power_request(
            &self,
            _: &InvocationReceiver,
            _: u64,
            _: bool,
        ) -> Result<(), crate::vm::service::Error> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(crate::vm::service::Error::NotSupported)
        }
        fn open_vcpu(
            &self,
            _: &InvocationReceiver,
            _: u32,
        ) -> Result<HandleValue, crate::vm::service::Error> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(crate::vm::service::Error::NotSupported)
        }
        fn virtual_machine_info(
            &self,
            _: &InvocationReceiver,
        ) -> Result<
            (
                crate::vm::objects::VirtualMachineConfiguration,
                crate::vm::objects::VirtualMachineSnapshot,
            ),
            crate::vm::service::Error,
        > {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(crate::vm::service::Error::NotSupported)
        }

        fn virtual_cpu_info(
            &self,
            _: &InvocationReceiver,
        ) -> Result<crate::vm::objects::VirtualCpuSnapshot, crate::vm::service::Error> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(crate::vm::service::Error::NotSupported)
        }

        fn set_virtual_cpu_affinity(
            &self,
            _: &InvocationReceiver,
            _: Option<UserSlice>,
            _: usize,
        ) -> Result<(), crate::vm::service::Error> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(crate::vm::service::Error::NotSupported)
        }

        fn start_virtual_cpu(
            &self,
            _: &InvocationReceiver,
        ) -> Result<(), crate::vm::service::Error> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(crate::vm::service::Error::NotSupported)
        }
    }

    impl SystemInspectServices for RejectingServices {
        fn memory_statistics(
            &self,
            _: &InvocationReceiver,
            _: crate::diagnostics::inspect::memory::Query,
        ) -> Result<
            crate::diagnostics::inspect::memory::Statistics,
            crate::diagnostics::inspect::Error,
        > {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(crate::diagnostics::inspect::Error::AccessDenied)
        }
        fn memory_observation(
            &self,
            _: &InvocationReceiver,
        ) -> Result<
            crate::diagnostics::inspect::MemoryObservation,
            crate::diagnostics::inspect::Error,
        > {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(crate::diagnostics::inspect::Error::AccessDenied)
        }

        fn cpu_observation(
            &self,
            _: &InvocationReceiver,
        ) -> Result<crate::task::scheduler::CpuTimeSnapshot, crate::diagnostics::inspect::Error>
        {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(crate::diagnostics::inspect::Error::AccessDenied)
        }
    }

    impl InspectServices for RejectingServices {
        fn read_object_details(
            &self,
            _inspector: &InvocationReceiver,
            _process: u64,
            _target: u64,
            _cursor: u64,
        ) -> Result<crate::object::diagnostics::ObjectDetails, crate::diagnostics::inspect::Error>
        {
            Err(crate::diagnostics::inspect::Error::Unavailable)
        }

        fn scan_processes(
            &self,
            _: &InvocationReceiver,
            _: u64,
            _: &mut Page<ProcessSnapshot, { crate::diagnostics::inspect::PROCESS_PAGE_CAPACITY }>,
        ) -> Result<(), crate::diagnostics::inspect::Error> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(crate::diagnostics::inspect::Error::AccessDenied)
        }

        fn scan_threads(
            &self,
            _: &InvocationReceiver,
            _: u64,
            _: &mut Page<TaskThreadSnapshot, { crate::diagnostics::inspect::THREAD_PAGE_CAPACITY }>,
        ) -> Result<(), crate::diagnostics::inspect::Error> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(crate::diagnostics::inspect::Error::AccessDenied)
        }

        fn scan_objects(
            &self,
            _: &InvocationReceiver,
            _: u64,
            _: &mut Page<crate::object::ObjectSnapshot, OBJECT_PAGE_CAPACITY>,
        ) -> Result<(), crate::diagnostics::inspect::Error> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(crate::diagnostics::inspect::Error::AccessDenied)
        }

        fn scan_process_handles(
            &self,
            _: &InvocationReceiver,
            _: u64,
            _: u64,
        ) -> Result<
            Page<ProcessHandleSnapshot, HANDLE_PAGE_CAPACITY>,
            crate::diagnostics::inspect::Error,
        > {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(crate::diagnostics::inspect::Error::AccessDenied)
        }

        fn derive_task_inspector(
            &self,
            _: &InvocationReceiver,
            _: HandleValue,
        ) -> Result<HandleValue, crate::diagnostics::inspect::Error> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(crate::diagnostics::inspect::Error::AccessDenied)
        }

        fn derive_object_inspector(
            &self,
            _: &InvocationReceiver,
            _: HandleValue,
        ) -> Result<HandleValue, crate::diagnostics::inspect::Error> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(crate::diagnostics::inspect::Error::AccessDenied)
        }

        fn derive_task_inspector_for_task_group(
            &self,
            _: &InvocationReceiver,
            _: HandleValue,
        ) -> Result<HandleValue, crate::diagnostics::inspect::Error> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(crate::diagnostics::inspect::Error::AccessDenied)
        }

        fn derive_object_inspector_for_task_group(
            &self,
            _: &InvocationReceiver,
            _: HandleValue,
        ) -> Result<HandleValue, crate::diagnostics::inspect::Error> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(crate::diagnostics::inspect::Error::AccessDenied)
        }

        fn derive_task_inspector_for_resource_domain(
            &self,
            _: &InvocationReceiver,
            _: HandleValue,
        ) -> Result<HandleValue, crate::diagnostics::inspect::Error> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(crate::diagnostics::inspect::Error::AccessDenied)
        }

        fn derive_object_inspector_for_resource_domain(
            &self,
            _: &InvocationReceiver,
            _: HandleValue,
        ) -> Result<HandleValue, crate::diagnostics::inspect::Error> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(crate::diagnostics::inspect::Error::AccessDenied)
        }
    }

    // These cases isolate payload decoding from admission. The mock service does
    // not downcast its receiver; real handle/kind/rights admission is tested by
    // the dispatch tests with actual object authorities.
    struct PayloadHarness {
        receiver: InvocationReceiver,
    }
    impl PayloadHarness {
        fn dispatch(
            &self,
            services: &impl super::services::NativeServices,
            payload: (NativeOperation, [u64; 6]),
        ) -> NativeAction {
            dispatch_operation(services, &self.receiver, payload.0, &payload.1)
        }
        fn returning(
            &self,
            services: &impl super::services::NativeServices,
            payload: (NativeOperation, [u64; 6]),
        ) -> Result<NativeResult, SelfTestError> {
            match self.dispatch(services, payload) {
                NativeAction::Return(result) => Ok(result),
                _ => Err(SelfTestError::DispatchAction),
            }
        }
    }
    fn invoke(operation: NativeOperation, arguments: [u64; 6]) -> (NativeOperation, [u64; 6]) {
        (operation, arguments)
    }
    let receiver_domain = crate::accounting::ResourceDomain::try_new_root(
        crate::accounting::ResourceLimits::UNLIMITED,
    )
    .map_err(|_| SelfTestError::RecordEncoding)?;
    let event = Event::try_new(&receiver_domain).map_err(|_| SelfTestError::RecordEncoding)?;
    let event = PublishableRef::try_new(event).map_err(|_| SelfTestError::RecordEncoding)?;
    let prepared = PreparedHandle::try_from_new_object(
        event.publication(),
        Rights::INSPECT,
        HandleFlags::NONE,
    )
    .map_err(|_| SelfTestError::RecordEncoding)?;
    let mut receiver_table = HandleTable::new(0x15a9_3d7b_4c62_8e01);
    let storage = receiver_table
        .reservation_storage_snapshot_for(1)
        .map_err(|_| SelfTestError::RecordEncoding)?;
    let mut storage =
        Some(HandleTableStoragePlan::try_new(storage).map_err(|_| SelfTestError::RecordEncoding)?);
    let reserved = receiver_table
        .reserve_with_plan::<1>(&mut storage)
        .map_err(|_| SelfTestError::RecordEncoding)?;
    let [handle] = reserved.publish(&mut receiver_table, [prepared]);
    let receiver = receiver_table.resolve_invocation(handle);
    receiver_table
        .remove(handle)
        .map_err(|_| SelfTestError::RecordEncoding)?
        .complete();
    let harness = PayloadHarness {
        receiver: receiver.map_err(|_| SelfTestError::RecordEncoding)?,
    };

    super::handlers::run_wire_self_test().map_err(|_| SelfTestError::RecordEncoding)?;

    let services = RejectingServices {
        calls: Cell::new(0),
    };
    // Malformed queue controls cannot allocate, publish, wait or consume a completion.
    {
        use hyper::abi::native::*;
        let queue_services = RejectingServices {
            calls: Cell::new(0),
        };
        for (operation, args) in [
            (NativeOperation::BlockQueueCreate, [0, 512, 1, 0, 0, 0]),
            (NativeOperation::BlockQueueCreate, [1, 511, 1, 0, 0, 0]),
            (NativeOperation::BlockQueueCreate, [1, 512, 5, 0, 0, 0]),
            (NativeOperation::BlockQueueCreate, [1, 512, 1, 1, 0, 0]),
            (NativeOperation::BlockQueueSubmit, [3, 1, 0x1000, 16, 0, 0]),
            (
                NativeOperation::BlockQueueSubmit,
                [0, 17, 0x1000, 272, 0, 0],
            ),
            (NativeOperation::BlockQueueSubmit, [2, 0, 0x1000, 0, 0, 0]),
            (
                NativeOperation::BlockQueueSubmit,
                [0, 1, 0x1000, HYPER_NATIVE_BLOCK_QUEUE_FRAME_BYTES + 1, 0, 0],
            ),
            (NativeOperation::BlockQueueWait, [1, 0, 1, 0, 0, 0]),
            (NativeOperation::BlockQueueWaitSpace, [1, 0, 0, 0, 0, 0]),
            (NativeOperation::BlockQueueWaitSpace, [512, 0, 1, 0, 0, 0]),
            (
                NativeOperation::BlockQueueWaitSpace,
                [HYPER_NATIVE_BLOCK_QUEUE_TRANSFER_BYTES + 512, 0, 0, 0, 0, 0],
            ),
            (NativeOperation::BlockQueueReap, [0, 0, 0, 0, 0, 0]),
            (
                NativeOperation::BlockQueueReap,
                [
                    1,
                    0x1000,
                    HYPER_NATIVE_BLOCK_QUEUE_TRANSFER_BYTES + 1,
                    0,
                    0,
                    0,
                ],
            ),
        ] {
            let result = harness.returning(&queue_services, invoke(operation, args))?;
            if result.status() != HYPER_NATIVE_STATUS_INVALID_ARGUMENT
                || queue_services.calls.get() != 0
            {
                return Err(SelfTestError::ChannelValidation);
            }
        }
        let result = harness.returning(
            &queue_services,
            invoke(NativeOperation::BlockQueueWait, [31, 0, 0, 0, 0, 0]),
        )?;
        if result.status() != HYPER_NATIVE_STATUS_OK
            || result.values() != &[31, HYPER_NATIVE_STATUS_IO_ERROR as u64]
            || queue_services.calls.get() != 1
        {
            return Err(SelfTestError::ChannelValidation);
        }
        let result = harness.returning(
            &queue_services,
            invoke(NativeOperation::BlockQueueWaitSpace, [0, 0, 0, 0, 0, 0]),
        )?;
        if result.status() != HYPER_NATIVE_STATUS_OK
            || result.values() != &[0, 0]
            || queue_services.calls.get() != 2
        {
            return Err(SelfTestError::ChannelValidation);
        }
        if super::wire::parse_object_kind(HYPER_NATIVE_OBJECT_BLOCK_QUEUE)
            != Ok(crate::object::ObjectKind::BLOCK_QUEUE)
        {
            return Err(SelfTestError::RecordEncoding);
        }
    }

    // Diagnostic reads validate selectors and bounded records before invoking
    // privileged services. Valid requests still require inspector authority.
    {
        use hyper::abi::native::*;
        let diagnostic_services = RejectingServices {
            calls: Cell::new(0),
        };
        let operation = NativeOperation::MemoryInspectorReadStatistics;
        let size = core::mem::size_of::<HyperNativeMemoryHeapStatistics>() as u64;
        let valid = [HYPER_NATIVE_MEMORY_STATISTICS_HEAP, 0, 0x2000, size, 0, 0];
        for (argument, value) in [
            (0, 0),
            (0, u64::MAX),
            (1, 1),
            (3, 0),
            (3, size - 1),
            (4, 1),
            (5, 1),
        ] {
            let mut args = valid;
            args[argument] = value;
            let NativeAction::Return(result) =
                harness.dispatch(&diagnostic_services, invoke(operation, args))
            else {
                return Err(SelfTestError::InvalidRecordSize);
            };
            if result.status() != HYPER_NATIVE_STATUS_INVALID_ARGUMENT
                || diagnostic_services.calls.get() != 0
            {
                return Err(SelfTestError::InvalidRecordSize);
            }
        }
        let NativeAction::Return(result) =
            harness.dispatch(&diagnostic_services, invoke(operation, valid))
        else {
            return Err(SelfTestError::InvalidRecordSize);
        };
        if result.status() != HYPER_NATIVE_STATUS_ACCESS_DENIED
            || diagnostic_services.calls.get() != 1
        {
            return Err(SelfTestError::InvalidRecordSize);
        }
    }

    let query = harness.returning(&services, invoke(NativeOperation::AbiQuery, [0; 6]))?;
    if query.status() != HYPER_NATIVE_STATUS_OK
        || query.values() != &[HYPER_NATIVE_ABI_REVISION, HYPER_NATIVE_FEATURE_CORE]
    {
        return Err(SelfTestError::AbiQuery);
    }
    let config = NativeOperation::SystemConfig;
    let page_key = hyper::abi::native::HYPER_NATIVE_SYSTEM_CONFIG_PAGE_SIZE;
    let page = harness.returning(&services, invoke(config, [page_key, 0, 0, 0, 0, 0]))?;
    if page.status() != HYPER_NATIVE_STATUS_OK || page.values() != &[hyper::mm::PAGE_SIZE, 0] {
        return Err(SelfTestError::AbiQuery);
    }
    for key in [0, u64::MAX] {
        let unknown = harness.returning(&services, invoke(config, [key, 0, 0, 0, 0, 0]))?;
        if unknown.status() != HYPER_NATIVE_STATUS_NOT_SUPPORTED || unknown.values() != &[0, 0] {
            return Err(SelfTestError::AbiQuery);
        }
    }
    for index in 1..6 {
        let mut args = [page_key, 0, 0, 0, 0, 0];
        args[index] = 1;
        let malformed = harness.returning(&services, invoke(config, args))?;
        if malformed.status() != HYPER_NATIVE_STATUS_INVALID_ARGUMENT {
            return Err(SelfTestError::AbiQuery);
        }
    }
    let first_clock = harness.returning(
        &services,
        invoke(NativeOperation::ClockGetMonotonic, [0; 6]),
    )?;
    let second_clock = harness.returning(
        &services,
        invoke(NativeOperation::ClockGetMonotonic, [0; 6]),
    )?;
    let malformed_clock = harness.returning(
        &services,
        invoke(NativeOperation::ClockGetMonotonic, [1, 0, 0, 0, 0, 0]),
    )?;
    if first_clock.status() != HYPER_NATIVE_STATUS_OK
        || first_clock.values()[1] != 0
        || second_clock.status() != HYPER_NATIVE_STATUS_OK
        || second_clock.values()[0] < first_clock.values()[0]
        || second_clock.values()[1] != 0
        || malformed_clock != failure(HYPER_NATIVE_STATUS_INVALID_ARGUMENT)
    {
        return Err(SelfTestError::Clock);
    }
    if harness.dispatch(&services, invoke(NativeOperation::ThreadYield, [0; 6]))
        != NativeAction::Yield(success([0, 0]))
        || harness.dispatch(
            &services,
            invoke(NativeOperation::ThreadExit, [u64::MAX, 0, 0, 0, 0, 0]),
        ) != (NativeAction::ExitThread { status: -1 })
        || harness.dispatch(
            &services,
            invoke(NativeOperation::ProcessExit, [42, 0, 0, 0, 0, 0]),
        ) != (NativeAction::ExitProcess { status: 42 })
    {
        return Err(SelfTestError::DispatchAction);
    }
    let thread_services = RejectingServices {
        calls: Cell::new(0),
    };
    for arguments in [[0; 6], [0x3000, 65, 0, 0, 0, 0], [0x3000, 1, 1, 0, 0, 0]] {
        if harness.returning(
            &thread_services,
            invoke(NativeOperation::ThreadSetCurrentName, arguments),
        )? != failure(HYPER_NATIVE_STATUS_INVALID_ARGUMENT)
            || thread_services.calls.get() != 0
        {
            return Err(SelfTestError::DispatchAction);
        }
    }
    for (pointer, count, expected) in [
        (1, 0, HYPER_NATIVE_STATUS_INVALID_ARGUMENT),
        (0, 1, HYPER_NATIVE_STATUS_INVALID_ARGUMENT),
        (0x2000, 5, HYPER_NATIVE_STATUS_INVALID_ARGUMENT),
        (u64::MAX, 1, HYPER_NATIVE_STATUS_FAULT),
    ] {
        if harness.dispatch(
            &thread_services,
            invoke(
                NativeOperation::ThreadCreate,
                [0x1000, 0x2000, 0, 7, pointer, count],
            ),
        ) != NativeAction::Return(failure(expected))
            || thread_services.calls.get() != 0
        {
            return Err(SelfTestError::ValidationReachedService);
        }
    }
    for (expected_calls, pointer, count) in [(1, 0, 0), (2, 0x3000, 1), (3, 0x3000, 4)] {
        if harness.dispatch(
            &thread_services,
            invoke(
                NativeOperation::ThreadCreate,
                [0x1000, 0x2000, 0, 7, pointer, count],
            ),
        ) != NativeAction::Return(failure(HYPER_NATIVE_STATUS_INVALID_ARGUMENT))
            || thread_services.calls.get() != expected_calls
        {
            return Err(SelfTestError::DispatchAction);
        }
    }
    let affinity_services = RejectingServices {
        calls: Cell::new(0),
    };
    for (arguments, expected) in [
        (
            [0x2000, 0, 0, 0, 0, 0],
            HYPER_NATIVE_STATUS_INVALID_ARGUMENT,
        ),
        (
            [0x2000, u64::MAX, 0, 0, 0, 0],
            HYPER_NATIVE_STATUS_INVALID_ARGUMENT,
        ),
        (
            [0x2000, 1, 1, 0, 0, 0],
            HYPER_NATIVE_STATUS_INVALID_ARGUMENT,
        ),
    ] {
        if harness.dispatch(
            &affinity_services,
            invoke(NativeOperation::VirtualCpuSetAffinity, arguments),
        ) != NativeAction::Return(failure(expected))
            || affinity_services.calls.get() != 0
        {
            return Err(SelfTestError::ValidationReachedService);
        }
    }
    if harness.dispatch(
        &affinity_services,
        invoke(
            NativeOperation::VirtualCpuSetAffinity,
            [0x2000, 1, 0, 0, 0, 0],
        ),
    ) != NativeAction::Return(failure(HYPER_NATIVE_STATUS_NOT_SUPPORTED))
        || affinity_services.calls.get() != 1
    {
        return Err(SelfTestError::DispatchAction);
    }
    for (error, status) in [
        (
            crate::task::scheduler::Error::CpuNotRegistered,
            HYPER_NATIVE_STATUS_INVALID_ARGUMENT,
        ),
        (
            crate::task::scheduler::Error::MigrationInProgress,
            HYPER_NATIVE_STATUS_BUSY,
        ),
        (
            crate::task::scheduler::Error::MigrationBlockedByCpuLocalWait,
            HYPER_NATIVE_STATUS_BUSY,
        ),
        (
            crate::task::scheduler::Error::TerminatedThread,
            HYPER_NATIVE_STATUS_BAD_STATE,
        ),
    ] {
        if status_from_vm_service_error(crate::vm::service::Error::from(
            crate::vm::objects::Error::Scheduler(error),
        )) != status
        {
            return Err(SelfTestError::VmErrorMapping);
        }
    }
    let timer_allocation = ObjectWaitError::Timer(TimedWaitError::Time(
        crate::time::Error::TimerQueue(crate::time::queue::TimerQueueError::Allocation),
    ));
    if status_from_object_wait_error(timer_allocation) != HYPER_NATIVE_STATUS_NO_MEMORY {
        return Err(SelfTestError::ObjectErrorMapping);
    }
    let quota_domain = crate::accounting::ResourceDomain::try_new_root(
        crate::accounting::ResourceLimits::UNLIMITED
            .with(crate::accounting::ResourceKind::Threads, 0),
    )
    .map_err(|_| SelfTestError::VmErrorMapping)?;
    {
        use crate::device::assigned::events::Events;
        use crate::object::SignalMask;
        use hyper::abi::native::{
            HYPER_NATIVE_SIGNAL_PHYSICAL_DEVICE_DMA_FAULT,
            HYPER_NATIVE_SIGNAL_PHYSICAL_DEVICE_READABLE,
        };
        let original = Events::new(&quota_domain).map_err(|_| SelfTestError::RecordEncoding)?;
        let fault_bit =
            SignalMask::from_trusted_bits(HYPER_NATIVE_SIGNAL_PHYSICAL_DEVICE_DMA_FAULT);
        if original.snapshot() != [0; 40] || original.signals.observe(fault_bit).is_some() {
            return Err(SelfTestError::RecordEncoding);
        }
        original.publish(hyper::abi::native::HyperNativeDeviceDmaFaultInfo {
            timestamp_ns: 123,
            address: 0x4560,
            stream_id: 16,
            source: 1,
            scope: 1,
            code: 0x10,
            flags: 5,
            reserved: 0,
        });
        let first = original.snapshot();
        let successor = original.clone();
        let weak = original.downgrade();
        drop(original);
        successor.publish(hyper::abi::native::HyperNativeDeviceDmaFaultInfo {
            timestamp_ns: 456,
            address: 0,
            stream_id: 16,
            source: 1,
            scope: 2,
            code: 1,
            flags: 0,
            reserved: 0,
        });
        let readable = SignalMask::from_trusted_bits(HYPER_NATIVE_SIGNAL_PHYSICAL_DEVICE_READABLE);
        successor
            .signals
            .update(SignalMask::EMPTY, readable)
            .map_err(|_| SelfTestError::RecordEncoding)?;
        successor
            .signals
            .update(readable, SignalMask::EMPTY)
            .map_err(|_| SelfTestError::RecordEncoding)?;
        if successor.snapshot() != first || successor.signals.observe(fault_bit).is_none() {
            return Err(SelfTestError::RecordEncoding);
        }
        drop(successor);
        if weak.upgrade().is_some() {
            return Err(SelfTestError::RecordEncoding);
        }
    }
    let quota_error = match quota_domain.reserve(
        crate::accounting::ResourceAmount::ZERO.with(crate::accounting::ResourceKind::Threads, 1),
    ) {
        Err(error) => error,
        Ok(reservation) => {
            drop(reservation);
            return Err(SelfTestError::VmErrorMapping);
        }
    };
    if status_from_vm_service_error(crate::vm::service::Error::from(
        crate::vm::objects::Error::Registry(crate::vm::registry::Error::Resource(quota_error)),
    )) != HYPER_NATIVE_STATUS_RESOURCE_LIMIT
        || status_from_vm_service_error(crate::vm::service::Error::from(
            crate::vm::objects::Error::Scheduler(crate::task::scheduler::Error::Thread(
                crate::task::thread::Error::Allocation,
            )),
        )) != HYPER_NATIVE_STATUS_NO_MEMORY
        || status_from_vm_service_error(crate::vm::service::Error::from(
            crate::vm::objects::Error::MemoryLayout(crate::vm::memory::Error::Resource(
                quota_error,
            )),
        )) != HYPER_NATIVE_STATUS_RESOURCE_LIMIT
        || status_from_vm_service_error(crate::vm::service::Error::from(
            crate::vm::objects::Error::MemoryLayout(crate::vm::memory::Error::MetadataAllocation),
        )) != HYPER_NATIVE_STATUS_NO_MEMORY
        || status_from_vm_service_error(crate::vm::service::Error::from(
            crate::vm::objects::Error::MemoryLayout(crate::vm::memory::Error::InvalidRange),
        )) != HYPER_NATIVE_STATUS_INVALID_ARGUMENT
        || status_from_vm_service_error(crate::vm::service::Error::from(
            crate::vm::objects::Error::Memory(crate::mm::user_space::MemoryObjectError::Vmo(
                crate::mm::user_space::VmoError::Busy,
            )),
        )) != HYPER_NATIVE_STATUS_BUSY
        || status_from_vm_service_error(crate::vm::service::Error::from(
            crate::vm::objects::Error::BadState,
        )) != HYPER_NATIVE_STATUS_BAD_STATE
    {
        return Err(SelfTestError::VmErrorMapping);
    }
    if status_from_byte_channel_error(ByteChannelError::WouldBlock)
        != HYPER_NATIVE_STATUS_WOULD_BLOCK
        || status_from_byte_channel_error(ByteChannelError::PeerClosed)
            != HYPER_NATIVE_STATUS_PEER_CLOSED
        || status_from_byte_channel_error(ByteChannelError::MessageTooLarge)
            != HYPER_NATIVE_STATUS_INVALID_ARGUMENT
    {
        return Err(SelfTestError::ChannelErrorMapping);
    }
    if status_from_capability_channel_error(CapabilityChannelError::WouldBlock)
        != HYPER_NATIVE_STATUS_WOULD_BLOCK
        || status_from_capability_channel_error(CapabilityChannelError::UserMemoryFault)
            != HYPER_NATIVE_STATUS_FAULT
        || status_from_capability_channel_error(CapabilityChannelError::BufferTooSmall {
            required_bytes: 1,
            required_handles: 1,
        }) != HYPER_NATIVE_STATUS_BUFFER_TOO_SMALL
        || status_from_capability_channel_error(CapabilityChannelError::Cancelled)
            != HYPER_NATIVE_STATUS_CANCELLED
    {
        return Err(SelfTestError::CapabilityChannelErrorMapping);
    }
    if capability_receive_result(Ok(CapabilityReceiveOutcome::Failed(
        CapabilityChannelError::BufferTooSmall {
            required_bytes: 4096,
            required_handles: 16,
        },
    ))) != NativeResult::for_operation(
        NativeOperation::CapabilityChannelReceive,
        HYPER_NATIVE_STATUS_BUFFER_TOO_SMALL,
        [4096, 16],
    ) {
        return Err(SelfTestError::CapabilityChannelErrorMapping);
    }
    if status_from_console_service_error(ConsoleServiceError::Io(
        crate::device::console::IoError::WouldBlock,
    )) != HYPER_NATIVE_STATUS_WOULD_BLOCK
        || console_io_result(Err(HYPER_NATIVE_STATUS_WOULD_BLOCK))
            != NativeResult::for_operation(
                NativeOperation::ConsoleRead,
                HYPER_NATIVE_STATUS_WOULD_BLOCK,
                [0, 0],
            )
    {
        return Err(SelfTestError::ConsoleErrorMapping);
    }
    let bad_rights = harness.dispatch(
        &services,
        invoke(NativeOperation::HandleDuplicate, [u64::MAX, 0, 0, 0, 0, 0]),
    );
    if bad_rights != NativeAction::Return(failure(HYPER_NATIVE_STATUS_INVALID_ARGUMENT)) {
        return Err(SelfTestError::InvalidRights);
    }
    let bad_size = harness.dispatch(
        &services,
        invoke(
            NativeOperation::HandleGetInfo,
            [0x2000, HANDLE_INFO_SIZE as u64 - 1, 0, 0, 0, 0],
        ),
    );
    if bad_size != NativeAction::Return(failure(HYPER_NATIVE_STATUS_INVALID_ARGUMENT)) {
        return Err(SelfTestError::InvalidRecordSize);
    }
    // Reject malformed generic-device requests before touching userspace or
    // invoking an authority/device operation. Sequence zero is deliberately
    // valid for an idle IRQ line resample, unlike malformed boolean values.
    let before_device_calls = services.calls.get();
    for (operation, arguments) in [
        (
            NativeOperation::DeviceFirmwareRead,
            [0x2000, 0, 65537, 0, 0, 0],
        ),
        (NativeOperation::DeviceFirmwareRead, [0x2000, 0, 0, 1, 0, 0]),
        (NativeOperation::DeviceClaimBundle, [0x2000, 0, 0, 0, 0, 0]),
        (NativeOperation::DeviceClaimBundle, [0x2000, 9, 0, 0, 0, 0]),
        (NativeOperation::DeviceMmio, [0, 8, 0, 0, 0, 0]),
        (NativeOperation::DeviceMmio, [0, 4, 2, 0, 0, 0]),
        (NativeOperation::DeviceMmio, [0, 4, 0, 1, 0, 0]),
        (NativeOperation::DeviceIrqPending, [1, 0, 0, 0, 0, 0]),
        (NativeOperation::DeviceIrqComplete, [0, 2, 0, 0, 0, 0]),
    ] {
        if harness.dispatch(&services, invoke(operation, arguments))
            != NativeAction::Return(failure(HYPER_NATIVE_STATUS_INVALID_ARGUMENT))
            || services.calls.get() != before_device_calls
        {
            return Err(SelfTestError::InvalidRecordSize);
        }
    }
    // The valid idle-line resample intentionally reaches the service. Keep its
    // counter separate so the final zero-call assertion still covers every
    // malformed request above and below without resetting accumulated evidence.
    let idle_irq_services = RejectingServices {
        calls: Cell::new(0),
    };
    if harness.dispatch(
        &idle_irq_services,
        invoke(NativeOperation::DeviceIrqComplete, [0, 0, 0, 0, 0, 0]),
    ) != NativeAction::Return(failure(HYPER_NATIVE_STATUS_NOT_SUPPORTED))
        || idle_irq_services.calls.get() != 1
    {
        return Err(SelfTestError::InvalidRecordSize);
    }
    let before_platform_calls = services.calls.get();
    for arguments in [
        [1, 0x2000, 23, 0, 0, 0],
        [u64::MAX, 0x2000, 24, 0, 0, 0],
        [1, 0x2000, 24, 1, 0, 0],
        [
            1,
            0x2000,
            HYPER_NATIVE_EXTENSIBLE_RECORD_MAX_BYTES + 1,
            0,
            0,
            0,
        ],
    ] {
        let result = harness.dispatch(
            &services,
            invoke(
                NativeOperation::VirtualMachineCreationLeaseGetPlatformInfo,
                arguments,
            ),
        );
        if result != NativeAction::Return(failure(HYPER_NATIVE_STATUS_INVALID_ARGUMENT))
            || services.calls.get() != before_platform_calls
        {
            return Err(SelfTestError::InvalidRecordSize);
        }
    }
    let before_namespace_calls = services.calls.get();
    for (operation, arguments) in [
        (
            NativeOperation::FilesystemMountRamfs,
            [0x2000, 0, 0, 0, 0, 0],
        ),
        (
            NativeOperation::FilesystemMountRamfs,
            [0x2000, 4097, 0, 0, 0, 0],
        ),
        (
            NativeOperation::FilesystemSwitchRoot,
            [0x2000, 1, 0, 0, 0, 0],
        ),
    ] {
        if harness.dispatch(&services, invoke(operation, arguments))
            != NativeAction::Return(failure(HYPER_NATIVE_STATUS_INVALID_ARGUMENT))
            || services.calls.get() != before_namespace_calls
        {
            return Err(SelfTestError::InvalidRecordSize);
        }
    }
    let before_private_calls = services.calls.get();
    for arguments in [
        [0x0f37_79b9_7f4a_7c12, 0x2000, 47, 0, 0, 0],
        [0x0f37_79b9_7f4a_7c12, 0x2000, 48, 1, 0, 0],
        [0x0f37_79b9_7f4a_7c12, 0x2000, 48, 0, 1, 0],
        [
            0x0f37_79b9_7f4a_7c12,
            0x2000,
            HYPER_NATIVE_EXTENSIBLE_RECORD_MAX_BYTES + 1,
            0,
            0,
            0,
        ],
    ] {
        let result = harness.dispatch(
            &services,
            invoke(NativeOperation::VmarMapPrivate, arguments),
        );
        if result != NativeAction::Return(failure(HYPER_NATIVE_STATUS_INVALID_ARGUMENT))
            || services.calls.get() != before_private_calls
        {
            return Err(SelfTestError::InvalidRecordSize);
        }
    }
    for arguments in [
        [
            0x0f37_79b9_7f4a_7c12,
            0x0f37_79b9_7f4a_7c13,
            0x2000,
            47,
            0,
            0,
        ],
        [
            0x0f37_79b9_7f4a_7c12,
            0x0f37_79b9_7f4a_7c13,
            0x2000,
            48,
            1,
            0,
        ],
        [
            0x0f37_79b9_7f4a_7c12,
            0x0f37_79b9_7f4a_7c13,
            0x2000,
            HYPER_NATIVE_EXTENSIBLE_RECORD_MAX_BYTES + 1,
            0,
            0,
        ],
    ] {
        if harness.dispatch(&services, invoke(NativeOperation::VmarMapFile, arguments))
            != NativeAction::Return(failure(HYPER_NATIVE_STATUS_INVALID_ARGUMENT))
            || services.calls.get() != before_private_calls
        {
            return Err(SelfTestError::InvalidRecordSize);
        }
    }
    if harness.dispatch(
        &services,
        invoke(NativeOperation::FileProtect, [1, 0, 0, 0, 0, 0]),
    ) != NativeAction::Return(failure(HYPER_NATIVE_STATUS_INVALID_ARGUMENT))
        || services.calls.get() != before_private_calls
    {
        return Err(SelfTestError::InvalidRecordSize);
    }
    // Valid mapping record coordinates must reach user-copy validation. A stale
    // receiver-prefixed adapter would instead reject address/size before the copy.
    for (operation, arguments) in [
        (
            NativeOperation::VmarMapPrivate,
            [0x0f37_79b9_7f4a_7c12, 0x2000, 48, 0, 0, 0],
        ),
        (
            NativeOperation::VmarMapFile,
            [
                0x0f37_79b9_7f4a_7c12,
                0x0f37_79b9_7f4a_7c13,
                0x2000,
                48,
                0,
                0,
            ],
        ),
    ] {
        let mapping_services = RejectingServices {
            calls: Cell::new(0),
        };
        if harness.dispatch(&mapping_services, invoke(operation, arguments))
            != NativeAction::Return(failure(HYPER_NATIVE_STATUS_NO_MEMORY))
            || mapping_services.calls.get() != 1
        {
            return Err(SelfTestError::InvalidRecordSize);
        }
    }
    // A future private-mapping record may grow only through a zero-filled tail.
    if copy_extensible_input_record::<48>(&RecordInput { nonzero_at: None }, 0x5000, 56, 48)
        .is_err()
        || copy_extensible_input_record::<48>(
            &RecordInput {
                nonzero_at: Some(0x5000 + 48),
            },
            0x5000,
            56,
            48,
        )
        .err()
            != Some(HYPER_NATIVE_STATUS_INVALID_ARGUMENT)
    {
        return Err(SelfTestError::InvalidRecordSize);
    }
    let compatible_info = prepare_info_request(
        &[0x2000, HANDLE_INFO_SIZE as u64 + 8, 0, 0, 0, 0],
        HYPER_NATIVE_HANDLE_INFO_MIN_SIZE,
        HANDLE_INFO_SIZE,
    )
    .map_err(|_| SelfTestError::InvalidRecordSize)?;
    if compatible_info.destination.length() != HANDLE_INFO_SIZE as u64
        || compatible_info.supported_size != HANDLE_INFO_SIZE
        || prepare_info_request(
            &[0x2000, HANDLE_INFO_SIZE as u64, 1, 0, 0, 0],
            HYPER_NATIVE_HANDLE_INFO_MIN_SIZE,
            HANDLE_INFO_SIZE,
        )
        .err()
            != Some(HYPER_NATIVE_STATUS_INVALID_ARGUMENT)
        || prepare_info_request(
            &[
                0x2000,
                HYPER_NATIVE_EXTENSIBLE_RECORD_MAX_BYTES + 1,
                0,
                0,
                0,
                0,
            ],
            HYPER_NATIVE_HANDLE_INFO_MIN_SIZE,
            HANDLE_INFO_SIZE,
        )
        .err()
            != Some(HYPER_NATIVE_STATUS_INVALID_ARGUMENT)
    {
        return Err(SelfTestError::InvalidRecordSize);
    }
    let prefix_info = prepare_info_request(&[0x3000, 8, 0, 0, 0, 0], 8, HANDLE_INFO_SIZE)
        .map_err(|_| SelfTestError::InvalidRecordSize)?;
    let output = RecordOutput {
        written: Cell::new(0),
    };
    if copy_info_record(&output, prefix_info, &[0; HANDLE_INFO_SIZE]) != Ok(HANDLE_INFO_SIZE as u64)
        || output.written.get() != 8
    {
        return Err(SelfTestError::InvalidRecordSize);
    }
    let resource_record_size = core::mem::size_of::<HyperNativeResourceLimits>() as u64;
    let extended_resource_arguments = [0x4000, resource_record_size + 8, 0, 0, 0, 0];
    if decode_resource_limits(
        &RecordInput { nonzero_at: None },
        &extended_resource_arguments,
    )
    .is_err()
        || decode_resource_limits(
            &RecordInput {
                nonzero_at: Some(0x4000 + resource_record_size),
            },
            &extended_resource_arguments,
        )
        .err()
            != Some(HYPER_NATIVE_STATUS_INVALID_ARGUMENT)
        || decode_resource_limits(
            &RecordInput { nonzero_at: None },
            &[0x4000, resource_record_size, 1, 0, 0, 0],
        )
        .err()
            != Some(HYPER_NATIVE_STATUS_INVALID_ARGUMENT)
    {
        return Err(SelfTestError::InvalidRecordSize);
    }
    let prefix = copy_extensible_input_record::<16>(
        &RecordInput {
            nonzero_at: Some(0x5003),
        },
        0x5000,
        8,
        8,
    )
    .map_err(|_| SelfTestError::InvalidRecordSize)?;
    if prefix[3] != 1
        || prefix[..3].iter().any(|byte| *byte != 0)
        || prefix[4..].iter().any(|byte| *byte != 0)
    {
        return Err(SelfTestError::InvalidRecordSize);
    }
    if copy_extensible_input_record::<16>(&RecordInput { nonzero_at: None }, 0x5000, 24, 8).is_err()
        || copy_extensible_input_record::<16>(
            &RecordInput {
                nonzero_at: Some(0x5000 + 16),
            },
            0x5000,
            24,
            8,
        )
        .err()
            != Some(HYPER_NATIVE_STATUS_INVALID_ARGUMENT)
    {
        return Err(SelfTestError::InvalidRecordSize);
    }
    let empty_wait_many = harness.dispatch(
        &services,
        invoke(NativeOperation::ObjectWaitMany, [0x2000, 0, 0, 0, 0, 0]),
    );
    let oversized_wait_many = harness.dispatch(
        &services,
        invoke(
            NativeOperation::ObjectWaitMany,
            [
                0x2000,
                HYPER_NATIVE_OBJECT_WAIT_MANY_MAX_ITEMS + 1,
                0,
                0,
                0,
                0,
            ],
        ),
    );
    let bad_process_info_size = harness.dispatch(
        &services,
        invoke(
            NativeOperation::ProcessGetInfo,
            [0x2000, PROCESS_INFO_SIZE as u64 - 1, 0, 0, 0, 0],
        ),
    );
    let bad_directory_page_capacity = harness.dispatch(
        &services,
        invoke(
            NativeOperation::DirectoryRead,
            [
                0,
                0x2000,
                HYPER_NATIVE_DIRECTORY_ENTRY_PAGE_CAPACITY - 1,
                0,
                0,
                0,
            ],
        ),
    );
    let bad_file_info_size = harness.dispatch(
        &services,
        invoke(
            NativeOperation::FileGetInfo,
            [
                0x2000,
                core::mem::size_of::<HyperNativeFileInfo>() as u64 - 1,
                0,
                0,
                0,
                0,
            ],
        ),
    );
    let bad_directory_info_size = harness.dispatch(
        &services,
        invoke(
            NativeOperation::DirectoryGetInfo,
            [
                0x2000,
                core::mem::size_of::<HyperNativeDirectoryInfo>() as u64 - 1,
                0,
                0,
                0,
                0,
            ],
        ),
    );
    if empty_wait_many != NativeAction::Return(failure(HYPER_NATIVE_STATUS_INVALID_ARGUMENT))
        || oversized_wait_many
            != NativeAction::Return(failure(HYPER_NATIVE_STATUS_INVALID_ARGUMENT))
        || bad_process_info_size
            != NativeAction::Return(failure(HYPER_NATIVE_STATUS_INVALID_ARGUMENT))
        || bad_directory_page_capacity
            != NativeAction::Return(failure(HYPER_NATIVE_STATUS_INVALID_ARGUMENT))
        || bad_file_info_size != NativeAction::Return(failure(HYPER_NATIVE_STATUS_INVALID_ARGUMENT))
        || bad_directory_info_size
            != NativeAction::Return(failure(HYPER_NATIVE_STATUS_INVALID_ARGUMENT))
    {
        return Err(SelfTestError::InvalidRecordSize);
    }
    // Malformed power control must not reach capability/lifecycle services.
    {
        use hyper::abi::native::NativeOperation::{
            VirtualMachineCompletePowerRequest as COMPLETE, VirtualMachineGetPowerRequest as GET,
            VirtualMachineOpenVcpu as OPEN,
        };
        let before = services.calls.get();
        for (operation, arguments) in [
            (GET, [0x2000, 39, 0, 0, 0, 0]),
            (GET, [0x2000, 4097, 0, 0, 0, 0]),
            (GET, [0x2000, 40, 1, 0, 0, 0]),
            (COMPLETE, [1, 2, 0, 0, 0, 0]),
            (COMPLETE, [0, 1, 0, 0, 0, 0]),
            (COMPLETE, [1, 1, 0, 0, 1, 0]),
            (OPEN, [u64::from(u32::MAX) + 1, 0, 0, 0, 0, 0]),
            (OPEN, [0, 1, 0, 0, 0, 0]),
        ] {
            if harness.dispatch(&services, invoke(operation, arguments))
                != NativeAction::Return(failure(HYPER_NATIVE_STATUS_INVALID_ARGUMENT))
            {
                return Err(SelfTestError::InvalidRecordSize);
            }
        }
        if services.calls.get() != before {
            return Err(SelfTestError::InvalidRecordSize);
        }
    }
    let bad_channel_create = harness.dispatch(
        &services,
        invoke(NativeOperation::ByteChannelCreate, [1, 0, 0, 0, 0, 0]),
    );
    let bad_channel_write = harness.dispatch(
        &services,
        invoke(NativeOperation::ByteChannelWrite, [1, 0, 0, 0, 0, 0]),
    );
    let bad_channel_read = harness.dispatch(
        &services,
        invoke(
            NativeOperation::ByteChannelRead,
            [
                0,
                0,
                HYPER_NATIVE_BYTE_CHANNEL_MAX_MESSAGE_BYTES + 1,
                0,
                0,
                0,
            ],
        ),
    );
    if bad_channel_create != NativeAction::Return(failure(HYPER_NATIVE_STATUS_INVALID_ARGUMENT))
        || bad_channel_write != NativeAction::Return(failure(HYPER_NATIVE_STATUS_INVALID_ARGUMENT))
        || bad_channel_read != NativeAction::Return(failure(HYPER_NATIVE_STATUS_INVALID_ARGUMENT))
    {
        return Err(SelfTestError::ChannelValidation);
    }
    let bad_capability_create = harness.dispatch(
        &services,
        invoke(NativeOperation::CapabilityChannelCreate, [1, 0, 0, 0, 0, 0]),
    );
    let bad_capability_send_options = harness.dispatch(
        &services,
        invoke(
            NativeOperation::CapabilityChannelTrySend,
            [1, 0, 0, 0, 0, 0],
        ),
    );
    let oversized_capability_send = harness.dispatch(
        &services,
        invoke(
            NativeOperation::CapabilityChannelTrySend,
            [
                0,
                0,
                HYPER_NATIVE_CAPABILITY_CHANNEL_MAX_MESSAGE_BYTES + 1,
                0,
                0,
                0,
            ],
        ),
    );
    let oversized_capability_receive = harness.dispatch(
        &services,
        invoke(
            NativeOperation::CapabilityChannelReceive,
            [
                0,
                0,
                0,
                0,
                HYPER_NATIVE_CAPABILITY_CHANNEL_MAX_HANDLES + 1,
                0,
            ],
        ),
    );
    let disposition_bytes = capability_disposition_bytes(3);
    let receive_slot_bytes = capability_receive_slot_bytes(5);
    // Unequal synthetic layouts prove the shared calculation remains generic;
    // send and receive records may evolve to different ABI sizes.
    let narrow_record_bytes = capability_record_bytes::<[u8; 7]>(3);
    let wide_record_bytes = capability_record_bytes::<[u8; 13]>(5);
    if bad_capability_create != NativeAction::Return(failure(HYPER_NATIVE_STATUS_INVALID_ARGUMENT))
        || bad_capability_send_options
            != NativeAction::Return(failure(HYPER_NATIVE_STATUS_INVALID_ARGUMENT))
        || oversized_capability_send
            != NativeAction::Return(failure(HYPER_NATIVE_STATUS_INVALID_ARGUMENT))
        || oversized_capability_receive
            != NativeAction::Return(failure(HYPER_NATIVE_STATUS_INVALID_ARGUMENT))
        || disposition_bytes
            != Ok(3 * core::mem::size_of::<HyperNativeCapabilityDisposition>() as u64)
        || receive_slot_bytes
            != Ok(5 * core::mem::size_of::<HyperNativeCapabilityReceiveSlot>() as u64)
        || narrow_record_bytes != Ok(21)
        || wide_record_bytes != Ok(65)
    {
        return Err(SelfTestError::CapabilityChannelValidation);
    }
    let bad_console_options = harness.dispatch(
        &services,
        invoke(NativeOperation::ConsoleRead, [1, 0, 0, 0, 0, 0]),
    );
    let oversized_console_write = harness.dispatch(
        &services,
        invoke(
            NativeOperation::ConsoleWrite,
            [
                0,
                0x2000,
                HYPER_NATIVE_CONSOLE_MAX_TRANSFER_BYTES + 1,
                0,
                0,
                0,
            ],
        ),
    );
    if bad_console_options != NativeAction::Return(failure(HYPER_NATIVE_STATUS_INVALID_ARGUMENT))
        || oversized_console_write
            != NativeAction::Return(failure(HYPER_NATIVE_STATUS_INVALID_ARGUMENT))
    {
        return Err(SelfTestError::ConsoleValidation);
    }
    let builder_authorities = [
        0x0f37_79b9_7f4a_7c12,
        0x0f37_79b9_7f4a_7c13,
        0x0f37_79b9_7f4a_7c14,
        0x0f37_79b9_7f4a_7c15,
        0,
        0,
    ];
    let decoded = parse_builder_create(&builder_authorities)
        .map_err(|_| SelfTestError::ProcessBuilderValidation)?;
    if [decoded.0, decoded.1, decoded.2, decoded.3].map(handle_wire::to_abi)
        != builder_authorities[..4]
    {
        return Err(SelfTestError::ProcessBuilderValidation);
    }
    let mut missing_loader = builder_authorities;
    missing_loader[3] = 0;
    if harness.dispatch(
        &services,
        invoke(NativeOperation::ProcessBuilderCreate, missing_loader),
    ) != NativeAction::Return(failure(HYPER_NATIVE_STATUS_BAD_HANDLE))
    {
        return Err(SelfTestError::ProcessBuilderValidation);
    }
    let oversized_builder_name = harness.dispatch(
        &services,
        invoke(
            NativeOperation::ProcessBuilderSetName,
            [0x2000, HYPER_NATIVE_PROCESS_NAME_MAX_BYTES + 1, 0, 0, 0, 0],
        ),
    );
    let oversized_affinity = harness.dispatch(
        &services,
        invoke(
            NativeOperation::ProcessBuilderSetAffinity,
            [
                0x2000,
                HYPER_NATIVE_PROCESS_AFFINITY_MAX_WORDS + 1,
                0,
                0,
                0,
                0,
            ],
        ),
    );
    let bad_process_builder_kind = harness.dispatch(
        &services,
        invoke(
            NativeOperation::ProcessBuilderAddHandle,
            [
                0x0f37_79b9_7f4a_7c12,
                1,
                u64::MAX,
                0,
                HYPER_NATIVE_CAPABILITY_DISPOSITION_MOVE,
                0,
            ],
        ),
    );
    let bad_process_builder_operation = harness.dispatch(
        &services,
        invoke(
            NativeOperation::ProcessBuilderAddHandle,
            [
                0x0f37_79b9_7f4a_7c12,
                1,
                u64::from(HYPER_NATIVE_OBJECT_EVENT),
                0,
                u64::MAX,
                0,
            ],
        ),
    );
    let same_rights_builder_handle = parse_builder_handle(&[
        0x0f37_79b9_7f4a_7c12,
        1,
        u64::from(HYPER_NATIVE_OBJECT_EVENT),
        HYPER_NATIVE_CAPABILITY_DISPOSITION_SAME_RIGHTS,
        HYPER_NATIVE_CAPABILITY_DISPOSITION_MOVE,
        0,
    ]);
    if oversized_builder_name != NativeAction::Return(failure(HYPER_NATIVE_STATUS_INVALID_ARGUMENT))
        || oversized_affinity != NativeAction::Return(failure(HYPER_NATIVE_STATUS_INVALID_ARGUMENT))
        || bad_process_builder_kind
            != NativeAction::Return(failure(HYPER_NATIVE_STATUS_INVALID_ARGUMENT))
        || bad_process_builder_operation
            != NativeAction::Return(failure(HYPER_NATIVE_STATUS_INVALID_ARGUMENT))
        || !matches!(
            same_rights_builder_handle,
            Ok((
                _,
                _,
                _,
                None,
                crate::capability::HandleTransferOperation::Move
            ))
        )
    {
        return Err(SelfTestError::ProcessBuilderValidation);
    }
    let handle_record = encode_handle_info_fields(0x1122_3344, 0x5566_7788, 0x99aa_bbcc_ddee_ff00);
    if handle_record
        != [
            0x44, 0x33, 0x22, 0x11, 0x88, 0x77, 0x66, 0x55, 0x00, 0xff, 0xee, 0xdd, 0xcc, 0xbb,
            0xaa, 0x99,
        ]
    {
        return Err(SelfTestError::RecordEncoding);
    }
    let object_record = encode_object_basic_info_fields(0x1122_3344_5566_7788, 0x99aa_bbcc);
    if object_record
        != [
            0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11, 0xcc, 0xbb, 0xaa, 0x99, 0, 0, 0, 0,
        ]
    {
        return Err(SelfTestError::RecordEncoding);
    }
    let process_record = encode_process_info_fields(
        0x1122_3344,
        0x5566_7788,
        0x99aa_bbcc_ddee_ff00,
        0x0123_4567_89ab_cdef,
    );
    if process_record
        != [
            0x44, 0x33, 0x22, 0x11, 0x88, 0x77, 0x66, 0x55, 0x00, 0xff, 0xee, 0xdd, 0xcc, 0xbb,
            0xaa, 0x99, 0xef, 0xcd, 0xab, 0x89, 0x67, 0x45, 0x23, 0x01, 0, 0, 0, 0, 0, 0, 0, 0,
        ]
    {
        return Err(SelfTestError::RecordEncoding);
    }
    let file_record = encode_file_info_fields(11, 17, 23, 29, 0o100_755);
    if file_record[0..8] != 11_u64.to_ne_bytes()
        || file_record[8..16] != 17_u64.to_ne_bytes()
        || file_record[16..24] != 23_u64.to_ne_bytes()
        || file_record[24..32] != 29_u64.to_ne_bytes()
        || file_record[32..36] != 0o100_755_u32.to_ne_bytes()
        || file_record[36..40] != [0; 4]
    {
        return Err(SelfTestError::RecordEncoding);
    }
    let directory_record = encode_directory_info_fields(31, 37, 0, 0o040_755);
    if directory_record[0..8] != 31_u64.to_ne_bytes()
        || directory_record[8..16] != 37_u64.to_ne_bytes()
        || directory_record[16..24] != 0_u64.to_ne_bytes()
        || directory_record[24..28] != 0o040_755_u32.to_ne_bytes()
        || directory_record[28..32] != [0; 4]
    {
        return Err(SelfTestError::RecordEncoding);
    }
    let inspection_domain = crate::accounting::ResourceDomain::try_new_root(
        crate::accounting::ResourceLimits::UNLIMITED,
    )
    .map_err(|_| SelfTestError::RecordEncoding)?;
    crate::object::test_delivery_rollback(&inspection_domain)
        .map_err(|_| SelfTestError::RecordEncoding)?;
    let event = Event::try_new(&inspection_domain).map_err(|_| SelfTestError::RecordEncoding)?;
    let service = PublishableRef::try_new(event).map_err(|_| SelfTestError::RecordEncoding)?;
    let active = service
        .publication()
        .activate()
        .map_err(|_| SelfTestError::RecordEncoding)?;
    let operation = active.pin::<Event>().ok_or(SelfTestError::RecordEncoding)?;
    let binding = operation.into_vm_device_binding();
    let inspection_record = encode_object_inspection(binding.snapshot());
    let binding_offset =
        core::mem::offset_of!(HyperNativeObjectInspection, vm_device_binding_references);
    let operation_offset = core::mem::offset_of!(HyperNativeObjectInspection, operation_references);
    if inspection_record[binding_offset..binding_offset + 8] != 1_u64.to_ne_bytes()
        || inspection_record[operation_offset..operation_offset + 8] != 0_u64.to_ne_bytes()
    {
        return Err(SelfTestError::RecordEncoding);
    }
    services.reached()
}
