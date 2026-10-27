// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Fallible construction of one dormant boot Process.

use crate::capability::handle_wire;

use crate::accounting::ResourceDomain;
use {crate::capability::HandleValue, crate::capability::PreparedHandle};
use {
    crate::process::PreparedProcess, crate::process::Process,
    crate::process::ProcessHandleReservation, crate::process::ProcessObject,
    crate::process::TaskGroup, crate::process::load_native,
};

use super::Error;

pub(super) struct BootProcess {
    pub(super) process: Process,
    executable: PreparedHandle,
}

// Image loading and authority installation are consecutive boot phases. Their
// temporary owners must not accumulate on the same scheduled kernel stack.
#[inline(never)]
pub(super) fn prepare(
    path: &'static str,
    thread_name: &'static str,
    group: &TaskGroup,
    domain: &ResourceDomain,
) -> Result<BootProcess, Error> {
    let root = crate::fs::vfs::root_directory(domain).map_err(Error::RootDirectory)?;
    let executable = root.open_file(path, domain).map_err(Error::RootDirectory)?;
    let executable = PreparedHandle::try_from_new_object(
        crate::object::ObjectPublication::try_new(executable).map_err(Error::Object)?,
        crate::authority::Rights::READ
            .union(crate::authority::Rights::EXECUTE)
            .union(crate::authority::Rights::PROTECT),
        crate::capability::HandleFlags::NONE,
    )
    .map_err(Error::Handle)?;
    let loader = root
        .open_file(loader_path()?, domain)
        .map_err(Error::RootDirectory)?;
    let loader = loader
        .executable_snapshot(domain)
        .map_err(Error::RootDirectory)?;
    let loaded = load_native(&loader, domain.clone()).map_err(Error::Image)?;
    let prepared = match PreparedProcess::try_new(
        loaded.image,
        group.clone(),
        domain.clone(),
        loaded.address_space,
    ) {
        Ok(prepared) => prepared,
        Err(failure) => {
            let (error, address_space) = failure.into_parts();
            if let Err(failure) =
                crate::mm::user_space::NativeAddressSpace::retire_unpublished(address_space)
            {
                let (cleanup_error, retained) = failure.into_parts();
                crate::pr_err!(
                    "HypeR: retaining unpublished boot-process address space after cleanup error: {cleanup_error:?}"
                );
                drop(retained);
            }
            return Err(Error::Process(error));
        }
    };
    let object = ProcessObject::try_service(prepared.process()).map_err(Error::TaskObject)?;
    let process = prepared.publish(
        object,
        crate::process::ProcessNameSnapshot::from_validated(thread_name),
    );
    Ok(BootProcess {
        process,
        executable,
    })
}

// Only the initial process has a kernel-selected loader. Descendants supply
// an explicit executable File capability through ProcessBuilder.
fn loader_path() -> Result<&'static str, Error> {
    use crate::hal::user::HostMachine;
    match crate::hal::user::host_machine() {
        HostMachine::Aarch64 => Ok("/lib/userspace-loader-hyper-aarch64"),
        HostMachine::Riscv64 => Ok("/lib/userspace-loader-hyper-riscv64"),
        _ => Err(Error::Image(
            crate::process::LoaderError::UnsupportedMachine,
        )),
    }
}

pub(super) fn install_handles<const N: usize>(
    boot: &BootProcess,
    purposes: [u32; N],
    prepare: impl FnOnce(&mut [Option<PreparedHandle>; N]) -> Result<(), Error>,
) -> Result<u64, Error> {
    let reservation = boot.process.reserve_handles::<N>()?;
    let values = reservation.values();
    // One owner table is filled in place. Partial preparation is automatically
    // retired on failure; no handle is visible before the batch publication.
    let mut slots = [const { None }; N];
    if let Err(error) = prepare(&mut slots) {
        drop(slots);
        boot.process.abort_handles(reservation);
        return Err(error);
    }
    let channel = match install_bootstrap(boot, &purposes, &values) {
        Ok(channel) => channel,
        Err(error) => {
            drop(slots);
            boot.process.abort_handles(reservation);
            return Err(error);
        }
    };
    finish_handles(boot, reservation, &values, &mut slots)?;
    Ok(channel)
}

// Authority construction can enter VFS. Keep the publication array and its
// failure-owned handles off that call chain; only this final phase consumes
// the complete slot table and the linear reservation.
#[inline(never)]
fn finish_handles<const N: usize>(
    boot: &BootProcess,
    reservation: ProcessHandleReservation<N>,
    values: &[HandleValue; N],
    slots: &mut [Option<PreparedHandle>; N],
) -> Result<(), Error> {
    let prepared = core::array::from_fn(|index| match slots[index].take() {
        Some(handle) => handle,
        None => hyper::debug::invariant_failure("init::bootstrap::finish_handles invariant"),
    });
    match boot.process.publish_handles(reservation, prepared) {
        Ok(published) if &published == values => Ok(()),
        Ok(_) => crate::diagnostics::crash::fatal(format_args!(
            "HypeR: startup handle publication changed reserved values"
        )),
        Err(failure) => Err(Error::Process(failure.error)),
    }
}

// Init has no parent awaiting a result. An empty opaque payload asks the
// installed userspace runtime to supply its own initial argument policy.
#[inline(never)]
fn install_bootstrap<const N: usize>(
    boot: &BootProcess,
    purposes: &[u32; N],
    values: &[HandleValue; N],
) -> Result<u64, Error> {
    use hyper::abi::native::*;
    let [execution_context, object_factory, system_info] =
        boot.process.prepare_invocation_handles()?;
    let reservation = boot.process.reserve_handles::<5>()?;
    let extra = reservation.values();
    let root = purposes
        .iter()
        .position(|p| u64::from(*p) == HYPER_NATIVE_STARTUP_HANDLE_PURPOSE_ROOT_VMAR);
    let stack = purposes
        .iter()
        .position(|p| u64::from(*p) == HYPER_NATIVE_STARTUP_HANDLE_PURPOSE_INITIAL_STACK_VMAR);
    let (root, stack) = match (root, stack) {
        (Some(root), Some(stack)) => (root, stack),
        _ => hyper::debug::invariant_failure("init startup VMARs missing"),
    };
    let layout = boot.process.image().bootstrap();
    let header = HyperNativeLoaderStartup {
        size: core::mem::size_of::<HyperNativeLoaderStartup>() as u32,
        handle_count: (N - 2) as u32,
        data_size: 0,
        flags: 0,
        executable: handle_wire::to_abi(extra[0]),
        root_vmar: handle_wire::to_abi(values[root]),
        stack_vmar: handle_wire::to_abi(values[stack]),
        stack_base: layout.stack_base,
        stack_size: layout.stack_size,
        loader_base: layout.loader_base,
        loader_size: layout.loader_size,
        execution_context: handle_wire::to_abi(extra[2]),
        object_factory: handle_wire::to_abi(extra[3]),
        system_info: handle_wire::to_abi(extra[4]),
    };
    let records = purposes
        .iter()
        .zip(values)
        .enumerate()
        .filter(|(i, _)| *i != root && *i != stack)
        .map(|(_, (purpose, value))| HyperNativeStartupHandle {
            purpose: *purpose,
            flags: 0,
            handle: handle_wire::to_abi(*value),
        });
    let prepared = match crate::process::bootstrap::prepare(
        &boot.process.resource_domain(),
        &boot.executable,
        header,
        records,
        &[],
    ) {
        Ok(prepared) => prepared,
        Err(error) => {
            boot.process.abort_handles(reservation);
            return Err(Error::Bootstrap(error));
        }
    };
    drop(prepared.parent_channel);
    let published = boot
        .process
        .publish_handles(
            reservation,
            [
                prepared.executable,
                prepared.child_channel,
                execution_context,
                object_factory,
                system_info,
            ],
        )
        .map_err(|failure| Error::Process(failure.error))?;
    Ok(handle_wire::to_abi(published[1]))
}
