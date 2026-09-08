// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Exercises checked copies through sparse stage-2 guest memory.

use hyper::mm::PAGE_SIZE;

use crate::kernel::vm::memory::{Error as MemoryError, GuestAddressSpace};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Error {
    Copy(MemoryError),
    DemandZero,
    Identity,
    InstructionPublication,
    Payload,
    Scheduler(crate::kernel::task::scheduler::Error),
    Resource(crate::kernel::accounting::ResourceError),
    Accounting,
    SnapshotAccounting,
    ReleaseAccounting,
    LimitAccounting,
    Statistics,
}

pub(super) fn run() -> Result<(), Error> {
    const BASE: u64 = 0x4000_0000;
    let identifier = crate::kernel::mm::translation_id::reserve::<
        crate::kernel::mm::translation_id::Stage2Vmid,
    >(8)
    .map_err(|_| Error::Identity)?;
    let domain = crate::kernel::accounting::ResourceDomain::try_new_root(
        crate::kernel::accounting::ResourceLimits::UNLIMITED,
    )
    .map_err(Error::Resource)?;
    let mut memory =
        GuestAddressSpace::new(identifier, BASE, 4 * PAGE_SIZE, &domain).map_err(Error::Copy)?;
    let (_, expected_metadata_bytes) =
        GuestAddressSpace::metadata_requirements_for_test(BASE, 4 * PAGE_SIZE)
            .map_err(Error::Copy)?;
    if memory
        .retained_metadata_bytes_for_test()
        .map_err(Error::Copy)?
        != expected_metadata_bytes
    {
        return Err(Error::Accounting);
    }
    let address = BASE + PAGE_SIZE - 16;

    let mut demand_zero = [0xff; 32];
    memory
        .copy_from(address, &mut demand_zero)
        .map_err(Error::Copy)?;
    if demand_zero != [0; 32] {
        return Err(Error::DemandZero);
    }

    let payload = *b"stage2 checked cross-page bytes";
    memory.copy_to(address, &payload).map_err(Error::Copy)?;
    let mut copied = [0; 31];
    memory
        .copy_from(address, &mut copied)
        .map_err(Error::Copy)?;
    if copied != payload {
        return Err(Error::Payload);
    }
    if memory.statistics().committed_pages != 2 {
        return Err(Error::Statistics);
    }
    if memory.instruction_page_ready_for_test(0) || memory.instruction_page_ready_for_test(1) {
        return Err(Error::InstructionPublication);
    }
    if domain
        .usage()
        .committed(crate::kernel::accounting::ResourceKind::GuestPages)
        != 2
    {
        return Err(Error::Accounting);
    }
    if memory.copy_to(BASE + 4 * PAGE_SIZE - 1, &[1, 2]).err() != Some(MemoryError::InvalidRange) {
        return Err(Error::Payload);
    }

    // Freeze the two resident payload pages, then populate another page. The
    // batch must publish exactly its frozen membership; a later active-leaf
    // preparation must independently publish a newly demand-created page.
    let memory_bytes_before_snapshot = domain
        .usage()
        .committed(crate::kernel::accounting::ResourceKind::KernelMemoryBytes);
    let snapshot = memory
        .instruction_snapshot_for_test()
        .map_err(Error::Copy)?;
    if domain
        .usage()
        .committed(crate::kernel::accounting::ResourceKind::KernelMemoryBytes)
        <= memory_bytes_before_snapshot
    {
        return Err(Error::SnapshotAccounting);
    }
    memory
        .copy_to(BASE + 2 * PAGE_SIZE, &[0x5a])
        .map_err(Error::Copy)?;
    if memory.instruction_page_ready_for_test(2) {
        return Err(Error::InstructionPublication);
    }
    let pin = crate::kernel::task::scheduler::preempt_disable().map_err(Error::Scheduler)?;
    memory
        .publish_instruction_snapshot_for_test(&pin, &snapshot)
        .map_err(Error::Copy)?;
    if !memory.instruction_page_ready_for_test(0)
        || !memory.instruction_page_ready_for_test(1)
        || memory.instruction_page_ready_for_test(2)
    {
        return Err(Error::InstructionPublication);
    }
    memory
        .prepare_active_leaf_page_for_test(3, &pin)
        .map_err(Error::Copy)?;
    if !memory.instruction_page_ready_for_test(3) {
        return Err(Error::InstructionPublication);
    }
    crate::kernel::task::scheduler::preempt_enable_without_reschedule(pin)
        .map_err(Error::Scheduler)?;
    let memory_bytes_with_snapshot = domain
        .usage()
        .committed(crate::kernel::accounting::ResourceKind::KernelMemoryBytes);
    let snapshot_bytes = snapshot.retained_metadata_bytes_for_test();
    drop(snapshot);
    let memory_bytes_after_snapshot = domain
        .usage()
        .committed(crate::kernel::accounting::ResourceKind::KernelMemoryBytes);
    if memory_bytes_after_snapshot.checked_add(snapshot_bytes) != Some(memory_bytes_with_snapshot) {
        return Err(Error::SnapshotAccounting);
    }
    drop(memory);
    for resource in [
        crate::kernel::accounting::ResourceKind::KernelMemoryBytes,
        crate::kernel::accounting::ResourceKind::CommittedPages,
        crate::kernel::accounting::ResourceKind::PinnedPages,
        crate::kernel::accounting::ResourceKind::GuestPages,
    ] {
        if domain.usage().total(resource) != 0 {
            return Err(Error::ReleaseAccounting);
        }
    }

    let limited = crate::kernel::accounting::ResourceDomain::try_new_root(
        crate::kernel::accounting::ResourceLimits::UNLIMITED
            .with(crate::kernel::accounting::ResourceKind::GuestPages, 1),
    )
    .map_err(Error::Resource)?;
    let identifier = crate::kernel::mm::translation_id::reserve::<
        crate::kernel::mm::translation_id::Stage2Vmid,
    >(8)
    .map_err(|_| Error::Identity)?;
    let mut limited_memory =
        GuestAddressSpace::new(identifier, BASE, 2 * PAGE_SIZE, &limited).map_err(Error::Copy)?;
    limited_memory.copy_to(BASE, &[1]).map_err(Error::Copy)?;
    if !matches!(
        limited_memory.copy_to(BASE + PAGE_SIZE, &[2]),
        Err(MemoryError::Resource(
            crate::kernel::accounting::ResourceError::LimitExceeded {
                resource: crate::kernel::accounting::ResourceKind::GuestPages,
                ..
            }
        ))
    ) {
        return Err(Error::LimitAccounting);
    }
    drop(limited_memory);
    if limited
        .usage()
        .total(crate::kernel::accounting::ResourceKind::GuestPages)
        != 0
    {
        return Err(Error::LimitAccounting);
    }

    // KernelOwned address spaces must admit their page-owner slot storage as
    // part of the initial metadata transaction. A limit which fits only the
    // shared-backing metadata must reject construction without retaining any
    // partial charge.
    let (common_metadata, kernel_owned_metadata) =
        GuestAddressSpace::metadata_requirements_for_test(BASE, 2 * PAGE_SIZE)
            .map_err(Error::Copy)?;
    if kernel_owned_metadata <= common_metadata {
        return Err(Error::LimitAccounting);
    }
    let metadata_limited = crate::kernel::accounting::ResourceDomain::try_new_root(
        crate::kernel::accounting::ResourceLimits::UNLIMITED.with(
            crate::kernel::accounting::ResourceKind::KernelMemoryBytes,
            kernel_owned_metadata - 1,
        ),
    )
    .map_err(Error::Resource)?;
    let identifier = crate::kernel::mm::translation_id::reserve::<
        crate::kernel::mm::translation_id::Stage2Vmid,
    >(8)
    .map_err(|_| Error::Identity)?;
    if !matches!(
        GuestAddressSpace::new(identifier, BASE, 2 * PAGE_SIZE, &metadata_limited),
        Err(MemoryError::Resource(
            crate::kernel::accounting::ResourceError::LimitExceeded {
                resource: crate::kernel::accounting::ResourceKind::KernelMemoryBytes,
                ..
            }
        ))
    ) || metadata_limited
        .usage()
        .total(crate::kernel::accounting::ResourceKind::KernelMemoryBytes)
        != 0
    {
        return Err(Error::LimitAccounting);
    }
    Ok(())
}
