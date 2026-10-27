// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Native machine probes using the production Process and VMO lifecycle.

#[cfg(CONFIG_ARCH_AARCH64)]
mod aarch64;
#[cfg(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64))]
mod fp;
#[cfg(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64))]
pub(super) use fp::guest_program as fp_guest_program;
#[cfg(CONFIG_ARCH_AARCH64)]
mod atomic_wait;
#[cfg(CONFIG_ARCH_RISCV64)]
mod riscv64;
#[cfg(CONFIG_ARCH_AARCH64)]
pub(super) use aarch64::run;
#[cfg(CONFIG_ARCH_RISCV64)]
pub(super) use riscv64::run;

use crate::capability::handle_wire;

use crate::accounting::ResourceDomain;
use hyper::mm::PAGE_SIZE;
use {
    crate::mm::user_space::UserAddress, crate::mm::user_space::UserSlice,
    crate::mm::user_space::prepare_native_entry_self_test,
};
use {
    crate::process::MachineAbi, crate::process::PreparedProcess, crate::process::Process,
    crate::process::ProcessImage, crate::process::ProcessObject, crate::process::ProcessPhase,
    crate::process::TaskGroup,
};

const IMAGE_BASE: u64 = 0x40_0000;
const EXECUTION_CONTEXT_SLOT: u64 = 16;
const SYSTEM_INFO_SLOT: u64 = 24;
const OBJECT_FACTORY_SLOT: u64 = 32;
#[cfg(CONFIG_ARCH_AARCH64)]
const EVENT_SLOT: u64 = 40;

/// Views one immutable position-independent assembly fixture.
///
/// # Safety
///
/// The labels must bound one retained linked section in ascending order.
unsafe fn program_bytes(start: *const u8, end: *const u8) -> &'static [u8] {
    let length = end.addr() - start.addr();
    // SAFETY: The caller supplies linked section boundaries; the bytes remain
    // allocated and immutable for the entire kernel lifetime.
    unsafe { core::slice::from_raw_parts(start, length) }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Error {
    AddressSpace,
    #[cfg(CONFIG_ARCH_AARCH64)]
    AtomicWait(&'static str),
    Construction,
    Group,
    Image,
    Lifecycle,
    Scheduler,
    Sleep(crate::task::SleepError),
    Terminal,
}

impl From<crate::task::SleepError> for Error {
    fn from(error: crate::task::SleepError) -> Self {
        Self::Sleep(error)
    }
}

fn prepare_process(
    domain: &ResourceDomain,
    group: &TaskGroup,
    program: &[u8],
    machine: MachineAbi,
) -> Result<Process, Error> {
    let code_range =
        UserSlice::new(UserAddress::new(IMAGE_BASE), PAGE_SIZE).map_err(|_| Error::Construction)?;
    let stack_range = UserSlice::new(UserAddress::new(IMAGE_BASE + PAGE_SIZE * 2), PAGE_SIZE)
        .map_err(|_| Error::Construction)?;
    let image_range = UserSlice::new(UserAddress::new(IMAGE_BASE), PAGE_SIZE * 3)
        .map_err(|_| Error::Construction)?;
    let address_space = prepare_native_entry_self_test(
        domain.clone(),
        image_range,
        code_range,
        stack_range,
        program,
    )
    .map_err(|_| Error::AddressSpace)?;
    let image = ProcessImage::try_native(
        machine,
        code_range.base(),
        stack_range.end(),
        UserAddress::new(0),
    )
    .map_err(|_| Error::Image)?;
    let prepared =
        match PreparedProcess::try_new(image, group.clone(), domain.clone(), address_space) {
            Ok(prepared) => prepared,
            Err(failure) => {
                let (_cause, address_space) = failure.into_parts();
                crate::mm::user_space::NativeAddressSpace::retire(address_space)
                    .map_err(|_| Error::AddressSpace)?;
                return Err(Error::Construction);
            }
        };
    let object = ProcessObject::try_service(prepared.process()).map_err(|_| Error::Construction)?;
    let process = prepared.publish(
        object,
        crate::process::ProcessNameSnapshot::from_validated("native-test"),
    );
    let handles = process
        .publish_invocation_handles()
        .map_err(|_| Error::Construction)?;
    for (offset, handle) in [
        (EXECUTION_CONTEXT_SLOT, handles.execution_context),
        (SYSTEM_INFO_SLOT, handles.system_info),
        (OBJECT_FACTORY_SLOT, handles.object_factory),
    ] {
        let destination = UserSlice::new(UserAddress::new(stack_range.base().get() + offset), 8)
            .map_err(|_| Error::AddressSpace)?;
        process
            .copy_to_user(destination, &handle_wire::to_abi(handle).to_le_bytes())
            .map_err(|_| Error::AddressSpace)?;
    }
    if !process_is_discoverable(&process) {
        return Err(Error::Construction);
    }
    process.start().map_err(|_| Error::Lifecycle)?;
    Ok(process)
}

#[cfg(CONFIG_ARCH_AARCH64)]
fn event_handle(process: &Process) -> Result<crate::capability::HandleValue, Error> {
    let source = UserSlice::new(UserAddress::new(IMAGE_BASE + PAGE_SIZE * 2 + EVENT_SLOT), 8)
        .map_err(|_| Error::AddressSpace)?;
    let mut bytes = [0; 8];
    process
        .copy_from_user(source, &mut bytes)
        .map_err(|_| Error::AddressSpace)?;
    crate::capability::handle_wire::from_abi(u64::from_le_bytes(bytes))
        .map_err(|_| Error::Construction)
}

fn process_is_discoverable(target: &Process) -> bool {
    let mut cursor = Some(crate::process::ProcessScanCursor::start());
    while let Some(position) = cursor {
        let page = crate::process::scan(position);
        if page
            .entries()
            .any(|entry| entry.snapshot().id == target.id())
        {
            return true;
        }
        cursor = page.next();
    }
    false
}

fn retire_process(process: &Process) -> Result<(), Error> {
    // Observation only: production kreaper must own and complete retirement.
    if crate::task::wait_for_test_progress(crate::task::TEST_PROGRESS_TIMEOUT_NS, || {
        Ok::<_, Error>(process.snapshot().phase == ProcessPhase::Retired)
    })? {
        Ok(())
    } else {
        Err(Error::Lifecycle)
    }
}

fn verify_thread_creation_affinity(
    domain: &ResourceDomain,
    group: &TaskGroup,
    program: &[u8],
    machine: MachineAbi,
) -> Result<(), Error> {
    let process = prepare_process(domain, group, program, machine)?;
    let foreign = prepare_process(domain, group, program, machine)?;
    let contexts = process.verify_invocation_context_for_test(&foreign);
    foreign.request_stop(crate::process::TerminalReason::Requested);
    retire_process(&foreign)?;
    if let Err(reason) = contexts {
        process.request_stop(crate::process::TerminalReason::Requested);
        retire_process(&process)?;
        crate::pr_err!("HypeR test: invocation context failed: {reason}");
        return Err(Error::Construction);
    }
    let caller = process
        .create_initial_user_thread(
            "selftest/affinity-parent",
            crate::task::scheduler::CpuMask::ALL,
        )
        .map_err(|_| Error::Scheduler)?;
    let result = crate::entry::verify_thread_affinity_creation_for_test(
        &process,
        &caller,
        UserAddress::new(IMAGE_BASE + PAGE_SIZE * 2),
    )
    .and_then(|()| crate::entry::verify_object_details_for_test(&process, &caller));
    process.request_stop(crate::process::TerminalReason::Requested);
    retire_process(&process)?;
    if let Err(reason) = result {
        crate::pr_err!("HypeR test: thread affinity creation failed: {reason}");
        return Err(Error::Scheduler);
    }
    crate::pr_info!("HypeR test: Native thread creation affinity passed");
    Ok(())
}
