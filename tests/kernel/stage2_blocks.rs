// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Actual `AArch64` descriptor tests, including allocation failure before BBM.
//! The hierarchy is never activated: these tests inspect the real descriptors
//! but do not claim to prove concurrent hardware walk or remote TLB behavior.

use alloc::vec::Vec;
use core::cell::Cell;
use hyper::mm::{PAGE_SIZE, PhysicalAddress};
use hyper::vm::translation::Stage2PagePermissions;

use crate::hal::vm::{Stage2AddressSpace, Stage2Error};
use crate::kernel::mm::page_block::PageBlock;

const BASE: u64 = 0x4000_0000;
const BLOCK: u64 = 2 * 1024 * 1024;
const RW: Stage2PagePermissions = Stage2PagePermissions::ReadWrite;
const RWX: Stage2PagePermissions = Stage2PagePermissions::ReadWriteExecute;

pub(super) enum Error {
    Allocation,
    Identity,
    Stage2(Stage2Error),
    Invariant(&'static str),
}

impl core::fmt::Debug for Error {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Allocation => formatter.write_str("Allocation"),
            Self::Identity => formatter.write_str("Identity"),
            Self::Stage2(error) => formatter.debug_tuple("Stage2").field(error).finish(),
            Self::Invariant(reason) => formatter.debug_tuple("Invariant").field(reason).finish(),
        }
    }
}

fn allocate_table(
    owners: &mut Vec<PageBlock>,
    pages: usize,
    alignment: usize,
) -> Option<PhysicalAddress> {
    if pages != 1 || alignment != 1 || owners.len() == owners.capacity() {
        return None;
    }
    let owner = PageBlock::allocate(0).ok()?;
    let physical = owner.physical();
    let address = crate::kernel::mm::memory::linear_address(physical.get())?;
    // SAFETY: The newly allocated page is exclusive and permanently mapped.
    // Retaining its owner below keeps it alive through every descriptor read.
    unsafe { core::ptr::write_bytes(address as *mut u8, 0, PAGE_SIZE as usize) };
    owners.push(owner);
    Some(physical)
}

fn expect_leaf(
    space: &Stage2AddressSpace,
    ipa: u64,
    expected: Option<(u64, u64, Stage2PagePermissions)>,
) -> Result<(), Error> {
    if space.normal_leaf_for_test(ipa).map_err(Error::Stage2)? != expected {
        return Err(Error::Invariant(
            "unexpected physical address, granule or permission",
        ));
    }
    Ok(())
}

pub(super) fn run() -> Result<(), Error> {
    let bits = crate::hal::vm::guest_translation_identifier_bits().map_err(|_| Error::Identity)?;
    let identifier = crate::kernel::mm::translation_id::reserve::<
        crate::kernel::mm::translation_id::Stage2Vmid,
    >(bits)
    .map_err(|_| Error::Identity)?
    .activate()
    .map_err(|_| Error::Identity)?;
    let lease = identifier.acquire().map_err(|_| Error::Identity)?;
    let backing = PageBlock::allocate(9).map_err(|_| Error::Allocation)?;
    let physical = backing.physical().get();
    let mut owners = Vec::new();
    owners
        .try_reserve_exact(12)
        .map_err(|_| Error::Allocation)?;
    let allocations = Cell::new(0usize);
    let mut allocate = |pages, alignment| {
        let physical = allocate_table(&mut owners, pages, alignment)?;
        allocations.set(allocations.get() + 1);
        Some(physical)
    };
    // SAFETY: Unique VMID reservation and exclusively retained zeroed pages.
    // This hierarchy is never activated or exposed to another execution context.
    let mut space = unsafe { Stage2AddressSpace::new(&mut allocate) }.map_err(Error::Stage2)?;
    // SAFETY: The test retains this lease through every temporary BBM selection.
    unsafe { space.bind_identifier(lease.value()) }.map_err(Error::Stage2)?;
    let initial_tables = allocations.get();
    // SAFETY: The whole aligned block is owned by backing and remains live;
    // neither this hierarchy nor its backing is published to a running VM.
    if !unsafe { space.try_map_normal_block(BASE, physical, RW, &mut allocate) }
        .map_err(Error::Stage2)?
    {
        return Err(Error::Invariant("fresh block was not installed"));
    }
    if allocations.get() != initial_tables + 1 {
        return Err(Error::Invariant(
            "block mapping allocated more than its L2 table",
        ));
    }
    expect_leaf(&space, BASE, Some((physical, BLOCK, RW)))?;
    expect_leaf(
        &space,
        BASE + BLOCK - PAGE_SIZE,
        Some((physical + BLOCK - PAGE_SIZE, BLOCK, RW)),
    )?;
    expect_leaf(&space, BASE + BLOCK, None)?;
    let mut fail = |_, _| None;
    // SAFETY: Republishing the exact owned block changes neither PA nor access.
    if !unsafe { space.try_map_normal_block(BASE, physical, RW, &mut fail) }
        .map_err(Error::Stage2)?
    {
        return Err(Error::Invariant("identical block was not idempotent"));
    }
    // SAFETY: The backing is retained; this deliberately conflicting request
    // must be rejected rather than broadening execute authority in place.
    if unsafe { space.try_map_normal_block(BASE, physical, RWX, &mut fail) }
        != Err(Stage2Error::Conflict)
    {
        return Err(Error::Invariant("conflicting block permissions accepted"));
    }
    expect_leaf(&space, BASE, Some((physical, BLOCK, RW)))?;

    // A partial clear without preparation must not silently remove neighbors.
    // SAFETY: This private hierarchy has no hardware consumers; owners survive.
    if unsafe { space.clear_normal_range(BASE + PAGE_SIZE, PAGE_SIZE) }
        != Err(Stage2Error::Conflict)
    {
        return Err(Error::Invariant("unprepared partial clear was accepted"));
    }
    // SAFETY: The sole hierarchy owner retains backing; failing allocator returns no pages.
    if unsafe { space.prepare_clear_normal_range(BASE + PAGE_SIZE, PAGE_SIZE, &mut fail) }
        != Err(Stage2Error::Allocation)
    {
        return Err(Error::Invariant("split allocation failure was lost"));
    }
    expect_leaf(
        &space,
        BASE + PAGE_SIZE,
        Some((physical + PAGE_SIZE, BLOCK, RW)),
    )?;
    // SAFETY: No guest can execute this private root; failed preparation publishes no permissions.
    if unsafe { space.make_normal_page_executable(BASE + PAGE_SIZE, &mut fail) }
        != Err(Stage2Error::Allocation)
    {
        return Err(Error::Invariant(
            "execute split allocation failure was lost",
        ));
    }
    expect_leaf(
        &space,
        BASE + PAGE_SIZE,
        Some((physical + PAGE_SIZE, BLOCK, RW)),
    )?;

    // The split preserves every neighboring physical page and XN permission.
    let before_split = allocations.get();
    let irq_enabled = crate::hal::irq::local_enabled();
    // SAFETY: Owned zeroed table allocations; this never-activated root is
    // inspected only, so no CPU can execute the test RAM or require I-cache publication.
    unsafe { space.make_normal_page_executable(BASE + PAGE_SIZE, &mut allocate) }
        .map_err(Error::Stage2)?;
    if irq_enabled != crate::hal::irq::local_enabled() {
        return Err(Error::Invariant("split changed local IRQ mask"));
    }
    if allocations.get() != before_split + 1 {
        return Err(Error::Invariant(
            "split did not allocate exactly one L3 table",
        ));
    }
    expect_leaf(&space, BASE, Some((physical, PAGE_SIZE, RW)))?;
    expect_leaf(
        &space,
        BASE + PAGE_SIZE,
        Some((physical + PAGE_SIZE, PAGE_SIZE, RWX)),
    )?;
    expect_leaf(
        &space,
        BASE + 2 * PAGE_SIZE,
        Some((physical + 2 * PAGE_SIZE, PAGE_SIZE, RW)),
    )?;
    expect_leaf(
        &space,
        BASE + BLOCK - PAGE_SIZE,
        Some((physical + BLOCK - PAGE_SIZE, PAGE_SIZE, RW)),
    )?;
    // SAFETY: The hierarchy and backing remain exclusively owned throughout preparation.
    unsafe { space.prepare_clear_normal_range(BASE + PAGE_SIZE, PAGE_SIZE, &mut fail) }
        .map_err(Error::Stage2)?;
    // SAFETY: Preparation and clearing are serialized by this sole owner.
    unsafe { space.clear_normal_range(BASE + PAGE_SIZE, PAGE_SIZE) }.map_err(Error::Stage2)?;
    expect_leaf(&space, BASE + PAGE_SIZE, None)?;
    expect_leaf(&space, BASE, Some((physical, PAGE_SIZE, RW)))?;

    // A retained L3 table is not promoted, even after all its leaves are clear.
    // SAFETY: The hierarchy and backing remain exclusively owned throughout preparation.
    unsafe { space.prepare_clear_normal_range(BASE, BLOCK, &mut fail) }.map_err(Error::Stage2)?;
    // SAFETY: Same private hierarchy, with its entire prepared interval owned.
    unsafe { space.clear_normal_range(BASE, BLOCK) }.map_err(Error::Stage2)?;
    // SAFETY: Stable owned backing, no competing table mutation or activation.
    if unsafe { space.try_map_normal_block(BASE, physical, RW, &mut allocate) }
        .map_err(Error::Stage2)?
    {
        return Err(Error::Invariant("empty L3 was unexpectedly promoted"));
    }
    // SAFETY: A page mapping remains valid after the block optimization declines.
    unsafe { space.map_normal_page(BASE, physical, RW, &mut allocate) }.map_err(Error::Stage2)?;
    expect_leaf(&space, BASE, Some((physical, PAGE_SIZE, RW)))?;

    // Whole-block revocation must not depend on allocating a split table.
    // SAFETY: Aliasing this private test-owned RAM adds no new authority.
    if !unsafe { space.try_map_normal_block(BASE + BLOCK, physical, RW, &mut allocate) }
        .map_err(Error::Stage2)?
    {
        return Err(Error::Invariant("second block was not installed"));
    }
    // The first page must survive if a later partial block makes clearing fail.
    // SAFETY: Sole ownership, no hardware users, and all backing remains live.
    if unsafe { space.clear_normal_range(BASE, BLOCK + PAGE_SIZE) } != Err(Stage2Error::Conflict) {
        return Err(Error::Invariant(
            "multi-leaf clear accepted a partial block",
        ));
    }
    expect_leaf(&space, BASE, Some((physical, PAGE_SIZE, RW)))?;
    // SAFETY: The hierarchy and backing remain exclusively owned throughout preparation.
    unsafe { space.prepare_clear_normal_range(BASE + BLOCK, BLOCK, &mut fail) }
        .map_err(Error::Stage2)?;
    // SAFETY: The entire block was prepared and no hardware can retain it.
    unsafe { space.clear_normal_range(BASE + BLOCK, BLOCK) }.map_err(Error::Stage2)?;
    expect_leaf(&space, BASE + BLOCK, None)?;
    expect_leaf(&space, BASE + 2 * BLOCK - PAGE_SIZE, None)?;
    // Device authority remains 4 KiB even for a whole aligned 2 MiB range.
    // SAFETY: Never-activated test aliases use owned RAM solely to inspect
    // descriptor encoding; no device or differently attributed access occurs.
    unsafe { space.map_device(BASE + 2 * BLOCK, physical, BLOCK, &mut allocate) }
        .map_err(Error::Stage2)?;
    if space
        .leaf_size_for_test(BASE + 2 * BLOCK)
        .map_err(Error::Stage2)?
        != Some(PAGE_SIZE)
        || space
            .leaf_size_for_test(BASE + 3 * BLOCK - PAGE_SIZE)
            .map_err(Error::Stage2)?
            != Some(PAGE_SIZE)
    {
        return Err(Error::Invariant("device mapping widened beyond 4 KiB"));
    }
    // SAFETY: Validation must reject device leaves before any descriptor store.
    if unsafe { space.clear_normal_range(BASE + 2 * BLOCK, PAGE_SIZE) }
        != Err(Stage2Error::Conflict)
    {
        return Err(Error::Invariant("normal revocation accepted device memory"));
    }
    // SAFETY: Owned test RAM is not executable by any CPU: this root remains
    // unpublished. Its execute bit is inspected only, with no I-cache consumer.
    if !unsafe { space.try_map_normal_block(BASE + 3 * BLOCK, physical, RWX, &mut allocate) }
        .map_err(Error::Stage2)?
    {
        return Err(Error::Invariant("executable block was not installed"));
    }
    // SAFETY: An already-executable leaf must remain a no-op, with no split.
    unsafe { space.make_normal_page_executable(BASE + 3 * BLOCK + PAGE_SIZE, &mut fail) }
        .map_err(Error::Stage2)?;
    expect_leaf(
        &space,
        BASE + 3 * BLOCK + PAGE_SIZE,
        Some((physical + PAGE_SIZE, BLOCK, RWX)),
    )?;
    // All storage can be reclaimed without a retirement rendezvous because no
    // guest ever selected this root. BBM restored the prior hardware selection.
    drop(lease);
    let retiring = identifier.begin_retirement().map_err(|_| Error::Identity)?;
    // SAFETY: The root was never executed and every temporary selection restored
    // the original registers; no CPU can retain a translation from this hierarchy.
    unsafe { retiring.complete() }.map_err(|_| Error::Identity)?;
    crate::pr_info!("HypeR test: stage-2 block mapping, split and revocation passed");
    Ok(())
}
