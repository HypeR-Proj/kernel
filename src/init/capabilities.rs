// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Kernel-owned bootstrap authorities for the initial Native process.

use crate::vm::objects::VirtualMachineCreationAuthority;
use {crate::accounting::ResourceDomain, crate::accounting::ResourceDomainObject};
use {
    crate::capability::HandleFlags, crate::capability::PreparedHandle, crate::capability::Rights,
};
use {
    crate::diagnostics::inspect::CpuInspector, crate::diagnostics::inspect::MemoryInspector,
    crate::diagnostics::inspect::ObjectInspector, crate::diagnostics::inspect::TaskInspector,
};
use {crate::mm::user_space, crate::mm::user_space::VmarObject, crate::mm::user_space::VmoObject};
use {
    crate::object::KernelObject, crate::object::ObjectPublication,
    crate::object::UserExportableObject,
};
use {crate::process::TaskFactory, crate::process::TaskGroup, crate::process::TaskGroupObject};

use super::Error;
use super::bootstrap::{self, BootProcess};

#[cfg(not(all(feature = "kernel-test", feature = "kernel-self-test")))]
const CORE_HANDLE_COUNT: usize = 12;
#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
const CORE_HANDLE_COUNT: usize = 11;

const HAS_VM_AUTHORITY: bool = crate::hal::vm::userspace_vm_lifecycle_available();
const HAS_DEVICE_AUTHORITY: bool = crate::device::assigned::available();
pub(super) const HANDLE_COUNT: usize =
    CORE_HANDLE_COUNT + HAS_VM_AUTHORITY as usize + HAS_DEVICE_AUTHORITY as usize;

const CORE_PURPOSES: [u32; CORE_HANDLE_COUNT] = [
    purpose(hyper::abi::native::HYPER_NATIVE_STARTUP_HANDLE_PURPOSE_RESOURCE_DOMAIN),
    purpose(hyper::abi::native::HYPER_NATIVE_STARTUP_HANDLE_PURPOSE_TASK_GROUP),
    purpose(hyper::abi::native::HYPER_NATIVE_STARTUP_HANDLE_PURPOSE_TASK_FACTORY),
    purpose(hyper::abi::native::HYPER_NATIVE_STARTUP_HANDLE_PURPOSE_ROOT_DIRECTORY),
    purpose(hyper::abi::native::HYPER_NATIVE_STARTUP_HANDLE_PURPOSE_TASK_INSPECTOR),
    purpose(hyper::abi::native::HYPER_NATIVE_STARTUP_HANDLE_PURPOSE_OBJECT_INSPECTOR),
    purpose(hyper::abi::native::HYPER_NATIVE_STARTUP_HANDLE_PURPOSE_MEMORY_INSPECTOR),
    purpose(hyper::abi::native::HYPER_NATIVE_STARTUP_HANDLE_PURPOSE_CPU_INSPECTOR),
    purpose(hyper::abi::native::HYPER_NATIVE_STARTUP_HANDLE_PURPOSE_ROOT_VMAR),
    purpose(hyper::abi::native::HYPER_NATIVE_STARTUP_HANDLE_PURPOSE_INITIAL_STACK_VMAR),
    purpose(hyper::abi::native::HYPER_NATIVE_STARTUP_HANDLE_PURPOSE_BOOT_COMMAND_LINE),
    #[cfg(not(all(feature = "kernel-test", feature = "kernel-self-test")))]
    purpose(hyper::abi::native::HYPER_NATIVE_STARTUP_HANDLE_PURPOSE_CONSOLE),
];

const PURPOSES: [u32; HANDLE_COUNT] = {
    let mut purposes = [0; HANDLE_COUNT];
    let mut index = 0;
    while index < CORE_HANDLE_COUNT {
        purposes[index] = CORE_PURPOSES[index];
        index += 1;
    }
    if HAS_VM_AUTHORITY {
        purposes[index] = purpose(
            hyper::abi::native::HYPER_NATIVE_STARTUP_HANDLE_PURPOSE_VIRTUAL_MACHINE_CREATION_AUTHORITY,
        );
        index += 1;
    }
    if HAS_DEVICE_AUTHORITY {
        purposes[index] = purpose(
            hyper::abi::native::HYPER_NATIVE_STARTUP_HANDLE_PURPOSE_DEVICE_ASSIGNMENT_AUTHORITY,
        );
    }
    purposes
};

// Keep the fixed bootstrap authority transaction out of the image-loading
// coordinator's frame; its storage is needed only after that phase completes.
#[inline(never)]
pub(super) fn install(
    init: &BootProcess,
    group: &TaskGroup,
    domain: &ResourceDomain,
) -> Result<u64, Error> {
    bootstrap::install_handles(init, PURPOSES, |handles| {
        prepare_handles(init, group, domain, handles)
    })
}

// Borrow the single owner table; constructor temporaries end before publication.
#[inline(never)]
fn prepare_handles(
    init: &BootProcess,
    group: &TaskGroup,
    domain: &ResourceDomain,
    handles: &mut [Option<PreparedHandle>; HANDLE_COUNT],
) -> Result<(), Error> {
    handles[0] = Some(prepare_handle(
        ResourceDomainObject::try_publication(domain.clone()).map_err(Error::ResourceObject)?,
        Rights::DUPLICATE
            .union(Rights::TRANSFER)
            .union(Rights::INSPECT)
            .union(Rights::CREATE_RESOURCE_DOMAIN)
            .union(Rights::SET_LIMITS)
            .union(Rights::REVOKE)
            .union(Rights::RESOURCE_DOMAIN_SPONSOR),
    )?);
    handles[1] = Some(prepare_handle(
        TaskGroupObject::try_publication(group.clone()).map_err(Error::TaskObject)?,
        Rights::DUPLICATE
            .union(Rights::WAIT)
            .union(Rights::TRANSFER)
            .union(Rights::INSPECT)
            .union(Rights::REQUEST_STOP)
            .union(Rights::TASK_GROUP_ATTACH_PROCESS),
    )?);
    handles[2] = Some(prepare_handle(
        ObjectPublication::try_new(TaskFactory::try_new(domain).map_err(Error::TaskObject)?)
            .map_err(Error::Object)?,
        Rights::DUPLICATE
            .union(Rights::TRANSFER)
            .union(Rights::INSPECT)
            .union(Rights::CREATE_PROCESS)
            .union(Rights::CREATE_TASK_GROUP)
            .union(Rights::RETAIN_PROCESS)
            .union(Rights::LOCK_MEMORY)
            .union(crate::task::scheduling_authority::SCHEDULING_RIGHTS),
    )?);
    let root = crate::fs::vfs::root_directory(domain).map_err(Error::RootDirectory)?;
    handles[3] = Some(prepare_handle(
        ObjectPublication::try_new(root).map_err(Error::Object)?,
        Rights::WRITE
            .union(Rights::PROTECT)
            .union(Rights::MANAGE_NAMESPACE)
            .union(Rights::SET_ATTRIBUTES)
            .union(Rights::LOCK_FILE)
            .union(Rights::DUPLICATE)
            .union(Rights::TRANSFER)
            .union(Rights::INSPECT)
            .union(Rights::READ)
            .union(Rights::EXECUTE),
    )?);
    handles[4] = Some(prepare_handle(
        ObjectPublication::try_new(TaskInspector::try_system(domain).map_err(Error::Inspection)?)
            .map_err(Error::Object)?,
        Rights::DUPLICATE
            .union(Rights::TRANSFER)
            .union(Rights::INSPECT)
            .union(Rights::DERIVE),
    )?);
    handles[5] = Some(prepare_handle(
        ObjectPublication::try_new(ObjectInspector::try_system(domain).map_err(Error::Inspection)?)
            .map_err(Error::Object)?,
        Rights::DUPLICATE
            .union(Rights::TRANSFER)
            .union(Rights::INSPECT)
            .union(Rights::INSPECT_DETAILS)
            .union(Rights::DERIVE),
    )?);
    handles[6] = Some(prepare_handle(
        ObjectPublication::try_new(MemoryInspector::try_system(domain).map_err(Error::Inspection)?)
            .map_err(Error::Object)?,
        Rights::DUPLICATE
            .union(Rights::TRANSFER)
            .union(Rights::INSPECT),
    )?);
    handles[7] = Some(prepare_handle(
        ObjectPublication::try_new(CpuInspector::try_system(domain).map_err(Error::Inspection)?)
            .map_err(Error::Object)?,
        Rights::DUPLICATE
            .union(Rights::TRANSFER)
            .union(Rights::INSPECT)
            .union(Rights::SET_ATTRIBUTES)
            .union(Rights::INSPECT_DETAILS),
    )?);
    if HAS_VM_AUTHORITY {
        *optional_authority_slot(handles, CORE_HANDLE_COUNT) = Some(prepare_handle(
            ObjectPublication::try_new(
                VirtualMachineCreationAuthority::try_new(domain)
                    .map_err(Error::VirtualMachineObject)?,
            )
            .map_err(Error::Object)?,
            VirtualMachineCreationAuthority::SUPPORTED_RIGHTS,
        )?);
    }
    #[cfg(not(all(feature = "kernel-test", feature = "kernel-self-test")))]
    {
        handles[11] = Some(prepare_handle(
            crate::device::console::SystemConsole::try_publication(domain)
                .map_err(Error::ConsoleObject)?,
            Rights::DUPLICATE
                .union(Rights::TRANSFER)
                .union(Rights::WAIT)
                .union(Rights::INSPECT)
                .union(Rights::READ)
                .union(Rights::WRITE),
        )?);
    }
    if HAS_DEVICE_AUTHORITY {
        *optional_authority_slot(handles, CORE_HANDLE_COUNT + HAS_VM_AUTHORITY as usize) =
            Some(prepare_handle(
                ObjectPublication::try_new(
                    crate::device::assigned::DeviceAssignmentAuthority::try_new(domain)
                        .map_err(Error::DeviceAssignment)?,
                )
                .map_err(Error::Object)?,
                Rights::DUPLICATE
                    .union(Rights::TRANSFER)
                    .union(Rights::INSPECT)
                    .union(Rights::ASSIGN_DEVICE)
                    .union(Rights::MAP_DMA),
            )?);
    }
    handles[10] = Some(prepare_boot_command_line(domain)?);
    // Prepare the root VMAR last. Its one-per-address-space publication claim
    // needs explicit rollback, while every earlier handle is self-contained.
    let address_space = init.process.address_space_owner()?;
    let token = init
        .process
        .image()
        .initial_stack_vmar()
        .ok_or(Error::MemoryObject(
            crate::mm::user_space::MemoryObjectError::AllocationSize,
        ))?;
    handles[9] = Some(prepare_handle(
        ObjectPublication::try_new(
            VmarObject::try_existing(address_space.clone(), token, domain)
                .map_err(Error::MemoryObject)?,
        )
        .map_err(Error::Object)?,
        VmarObject::ROOT_RIGHTS,
    )?);
    let root_vmar_publication = VmarObject::try_root_publication(address_space.clone(), domain)
        .map_err(Error::MemoryObject)?;
    let root_vmar = match PreparedHandle::try_from_new_object(
        root_vmar_publication,
        VmarObject::ROOT_RIGHTS,
        HandleFlags::NONE,
    ) {
        Ok(handle) => handle,
        Err(error) => {
            VmarObject::abort_root_publication(&address_space);
            return Err(Error::Handle(error));
        }
    };
    handles[8] = Some(root_vmar);
    Ok(())
}

fn optional_authority_slot(
    handles: &mut [Option<PreparedHandle>; HANDLE_COUNT],
    index: usize,
) -> &mut Option<PreparedHandle> {
    // The count and optional capabilities share the same machine constants.
    // Checked access also handles architectures where an entire branch is
    // disabled and its hypothetical index equals the end of the core table.
    match handles.get_mut(index) {
        Some(slot) => slot,
        None => {
            hyper::debug::invariant_failure("init::capabilities::optional_authority_slot invariant")
        }
    }
}

// Keep the bounded payload buffer off the bootstrap transaction's stack frame.
#[inline(never)]
fn prepare_boot_command_line(domain: &ResourceDomain) -> Result<PreparedHandle, Error> {
    const MAX: usize = hyper::abi::native::HYPER_NATIVE_BOOT_COMMAND_LINE_MAX_BYTES as usize;
    const _: () = assert!(hyper_platform::chosen::MAX_COMMAND_LINE_SIZE <= MAX);
    let source = crate::boot::with_command_line(|command_line| {
        let text = command_line.map_or("", |value| value.as_str());
        let mut bytes = [0_u8; 8 + MAX];
        bytes[..8].copy_from_slice(&(text.len() as u64).to_le_bytes());
        bytes[8..8 + text.len()].copy_from_slice(text.as_bytes());
        user_space::SnapshotVmo::try_from_bytes(
            &bytes[..8 + text.len()],
            user_space::KernelPageBackend,
            user_space::DomainAccount::new(domain.clone()),
        )
    })
    .map_err(|error| Error::MemoryObject(user_space::MemoryObjectError::Vmo(error)))?;
    let object = VmoObject::from_snapshot(source, domain).map_err(Error::MemoryObject)?;
    prepare_handle(
        ObjectPublication::try_new(object).map_err(Error::Object)?,
        Rights::READ.union(Rights::INSPECT),
    )
}

fn prepare_handle<T: UserExportableObject>(
    publication: ObjectPublication<T>,
    rights: Rights,
) -> Result<PreparedHandle, Error> {
    PreparedHandle::try_from_new_object(publication, rights, HandleFlags::NONE)
        .map_err(Error::Handle)
}

const fn purpose(purpose: u64) -> u32 {
    assert!(purpose <= u32::MAX as u64);
    purpose as u32
}
