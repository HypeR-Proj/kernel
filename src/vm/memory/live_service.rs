// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Native ownership of a backend VM's one-shot device mapping.

use crate::accounting::CommittedCharge;
use crate::authority::Rights;
use crate::process::Process;
use {
    crate::capability::HandleFlags, crate::capability::HandleValue,
    crate::capability::InvocationReceiver, crate::capability::PreparedHandle,
};
use {
    crate::object::KernelObject, crate::object::ObjectKind, crate::object::ObjectPublication,
    crate::object::TransferClass, crate::object::private,
};
use {
    crate::vm::objects::GuestMemoryObject, crate::vm::objects::VirtualMachineObject,
    crate::vm::registry, crate::vm::service::Error,
};

pub(crate) struct GuestMapping {
    backend: registry::VmId,
    token: u64,
    _charge: CommittedCharge,
}
impl private::Sealed for GuestMapping {}
impl private::UserExportable for GuestMapping {}
impl KernelObject for GuestMapping {
    const KIND: ObjectKind = ObjectKind::GUEST_MAPPING;
    const TRANSFER_CLASS: TransferClass = TransferClass::RendezvousOnly;
    const SUPPORTED_RIGHTS: Rights = Rights::WRITE.union(Rights::TRANSFER).union(Rights::INSPECT);
    // Closing a handle cannot establish that a device has stopped DMA. The VM
    // owns the record until explicit guest-proven release or VM retirement.
}

fn classify(error: super::Error) -> Error {
    crate::vm::objects::Error::MemoryLayout(error).into()
}

pub(crate) fn create(
    process: &Process,
    backend: &InvocationReceiver,
    memory: HandleValue,
    frontend: u64,
) -> Result<(HandleValue, u64), Error> {
    if !crate::hal::vm::supports_io_notifications() {
        return Err(Error::NotSupported);
    }
    let vm = backend
        .resolve::<VirtualMachineObject>(Rights::WRITE)
        .map_err(crate::process::ProcessError::from)?;
    let memory = process.resolve_handle::<GuestMemoryObject>(memory, Rights::MAP)?;
    let domain = process.resource_domain();
    let owner = vm.object().owner();
    let id = owner.io_update_id().map_err(|_| Error::BadState)?;
    let binding = registry::acquire_binding(id).map_err(|_| Error::BadState)?;
    let mut mapping = super::live::Mapping::prepare(memory.object().backing(), frontend, &domain)
        .map_err(classify)?;
    let token = binding
        .with_address_space(|space| space.live.reserve_token())
        .map_err(classify)?;
    mapping.token = token;
    let dma = binding.dma();
    let dma_lease = mapping.lease.clone();
    let mut mapping = Some(hyper::mm::try_box(mapping).map_err(|_| Error::NoMemory)?);
    let charge =
        crate::vm::objects::reserve_object_charge::<GuestMapping>(&domain).map_err(Error::from)?;
    let reservation = process.reserve_handles::<1>()?;
    let result = (|| {
        let publication = ObjectPublication::try_new(GuestMapping {
            backend: id,
            token,
            _charge: charge,
        })
        .map_err(|_| Error::NoMemory)?;
        let prepared = PreparedHandle::try_from_new_object(
            publication,
            GuestMapping::SUPPORTED_RIGHTS,
            HandleFlags::NONE,
        )
        .map_err(|_| Error::NoMemory)?;
        if let Some(dma) = &dma {
            dma.grant(token, &dma_lease).map_err(|_| Error::BadState)?;
        }
        let mut committed = false;
        for _ in 0..8 {
            // Retry only unpublished metadata preparation. Reclaim may sleep,
            // so every attempt releases the address-space lock first and
            // revalidates the candidate after the wait.
            crate::mm::reclaim::retry_prepare_with(
                || {
                    binding.with_address_space(|space| {
                        space.prepare_live_install(
                            mapping.as_ref().ok_or(super::Error::InvalidRange)?,
                        )
                    })
                },
                |failure| failure.reclaim,
            )
            .map_err(|failure| classify(failure.error))?;
            let installed = owner
                .with_io_update(|current| {
                    if current != id {
                        return Err(crate::vm::io::Error::BadState);
                    }
                    Ok(binding.with_address_space(|space| space.install_live(&mut mapping)))
                })
                .map_err(|_| Error::BadState)?
                .map_err(classify)?;
            if installed.is_some() {
                committed = true;
                break;
            }
        }
        if !committed {
            return Err(Error::WouldBlock);
        }
        Ok((prepared, token))
    })();
    let (prepared, token) = match result {
        Ok(value) => value,
        Err(error) => {
            process.abort_handles(reservation);
            if let Some(dma) = &dma {
                // No CPU admission was published on this error path. Failed
                // invalidation retains the driver's counted backing owners.
                let _ = dma.revoke(token);
            }
            return Err(error);
        }
    };
    match process.publish_handles(reservation, [prepared]) {
        Ok(handles) => Ok((handles[0], token)),
        Err(failure) => {
            // A never-admitted token cannot have published a leaf or acquired
            // a device reference. Cancel under the same lock as guest admit.
            // If the guest already admitted it, retain the record as quarantine;
            // handle publication failure never fabricates a DMA proof.
            let cancelled = binding.with_address_space(|space| {
                let slot = space
                    .live
                    .slots
                    .iter_mut()
                    .find(|slot| slot.as_ref().is_some_and(|record| record.token == token))?;
                if slot.as_ref()?.state != super::grant_state::State::Created {
                    return None;
                }
                slot.take()
            });
            if cancelled.is_some()
                && let Some(dma) = &dma
            {
                let _ = dma.revoke(token);
            }
            drop(cancelled);
            Err(failure.error.into())
        }
    }
}

pub(crate) fn release(handle: &InvocationReceiver) -> Result<(), Error> {
    let mapping = handle
        .resolve::<GuestMapping>(Rights::WRITE)
        .map_err(crate::process::ProcessError::from)?;
    let mapping = mapping.object();
    let binding = registry::acquire_binding(mapping.backend).map_err(|_| Error::BadState)?;
    let mut synchronization =
        super::prepare_live_synchronization(&binding).map_err(|error| match error {
            super::SynchronizationError::Unsupported => Error::NotSupported,
            super::SynchronizationError::TransportBusy => Error::WouldBlock,
            super::SynchronizationError::TopologyUnavailable => Error::BadState,
            super::SynchronizationError::Memory(error) => classify(error),
        })?;
    binding.with_address_space(|space| {
        let record = space.live.find_mut(mapping.token).ok_or(Error::BadState)?;
        if !record.state.can_retire() {
            return Err(Error::Busy);
        }
        // Prepare every extent before withdrawing admission. Splitting a
        // boundary block preserves its exact backing and permissions; a
        // preparation failure therefore leaves this grant retryable. Whole
        // blocks require no allocation, including during memory pressure.
        for extent in &record.extents {
            let mut allocator =
                |pages, alignment| space.table_pages.allocate_zeroed(pages, alignment);
            // SAFETY: The address-space lock serializes mutation, the table
            // pool retains every new table, and the grant retains backing.
            if let Err(error) = unsafe {
                space
                    .stage2
                    .prepare_clear_normal_range(extent.alias, extent.length, &mut allocator)
            } {
                return Err(classify(
                    space
                        .table_pages
                        .take_error()
                        .unwrap_or_else(|| error.into()),
                ));
            }
        }
        if !record.state.retire() {
            crate::diagnostics::crash::fatal(format_args!(
                "HypeR: prepared live grant lost retirement eligibility"
            ));
        }
        for extent in &record.extents {
            // SAFETY: All ranges were prepared under this same lock. Lookup
            // is now withdrawn, and backing survives the all-CPU barrier.
            if unsafe { space.stage2.clear_normal_range(extent.alias, extent.length) }.is_err() {
                crate::diagnostics::crash::fatal(format_args!(
                    "HypeR: prepared live grant stage-2 removal failed"
                ));
            }
        }
        // Faults may retry, but must not rebuild an invalid parent slot before
        // the break-before-make barrier has completed on every CPU.
        space.table_rebuild_blocked = true;
        // SAFETY: The address-space lock excludes software writers. Detached
        // owners remain in the pool through the following all-CPU rendezvous.
        if unsafe {
            space
                .stage2
                .detach_empty_tables(&mut |page| space.table_pages.detach(page))
        }
        .is_err()
        {
            crate::diagnostics::crash::fatal(format_args!(
                "HypeR: stage-2 table detachment failed"
            ));
        }
        Ok(())
    })?;
    synchronization.execute();
    loop {
        let retired = binding.with_address_space(|space| {
            // SAFETY: The serialized invalidation completed on every frozen
            // CPU. LiveSynchronization excludes a second retirement until
            // this pool's acknowledged owners have all been removed.
            unsafe { space.table_pages.take_retired() }
        });
        let Some((page, charge)) = retired else {
            break;
        };
        drop(page);
        drop(charge);
    }
    binding.with_address_space(|space| space.table_rebuild_blocked = false);
    drop(synchronization);
    if let Some(dma) = binding.dma() {
        dma.revoke(mapping.token).map_err(|_| Error::BadState)?;
    }
    let retired = binding.with_address_space(|space| {
        space
            .live
            .slots
            .iter_mut()
            .find(|slot| {
                slot.as_ref()
                    .is_some_and(|record| record.token == mapping.token)
            })
            .and_then(Option::take)
    });
    drop(retired);
    let spare = binding.with_address_space(|space| {
        let capacity = space.live.table_capacity().ok()?;
        space.table_pages.compact_live(capacity)
    });
    drop(spare);
    Ok(())
}
