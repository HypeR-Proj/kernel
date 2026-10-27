// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Runtime checks of Native machine roots and pageable backing.

use super::*;
use alloc::boxed::Box;
use alloc::vec::Vec;

#[cfg(CONFIG_ARCH_AARCH64)]
mod busy_detach;
#[cfg(CONFIG_ARCH_AARCH64)]
pub(super) use busy_detach::{after_cut, observe_busy};

struct TestFile {
    bytes: Vec<u8>,
    reads: FallibleArc<AtomicU64>,
}
impl super::super::vmo::FilePageSource<KernelPageBackend> for TestFile {
    fn size(&self) -> u64 {
        self.bytes.len() as u64
    }
    fn read_at(&self, offset: u64, output: &mut [u8]) -> Result<usize, ()> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        let offset = usize::try_from(offset).map_err(|_| ())?;
        let bytes = self.bytes.get(offset..).ok_or(())?;
        let count = output.len().min(bytes.len());
        output[..count].copy_from_slice(&bytes[..count]);
        Ok(count)
    }
}

fn source(bytes: &[u8], reads: FallibleArc<AtomicU64>) -> Result<Box<TestFile>, Error> {
    let mut owned = Vec::new();
    owned
        .try_reserve_exact(bytes.len())
        .map_err(|_| Error::Allocation)?;
    owned.extend_from_slice(bytes);
    hyper::mm::try_box(TestFile {
        bytes: owned,
        reads,
    })
    .map_err(|_| Error::Allocation)
}

fn require(condition: bool) -> Result<(), Error> {
    if condition {
        Ok(())
    } else {
        Err(Error::Unsupported)
    }
}

impl NativeAddressSpace {
    /// Installs an untouched private output page for a real EL0 copyout probe.
    #[cfg(CONFIG_ARCH_AARCH64)]
    pub(crate) fn prepare_cow_output_for_test(&self, range: UserSlice) -> Result<(), Error> {
        let reads = FallibleArc::try_new(AtomicU64::new(0)).map_err(|_| Error::Allocation)?;
        static BYTES: [u8; PAGE_SIZE as usize] = [0x5a; PAGE_SIZE as usize];
        let logical = self.logical();
        let view = logical.prepare_file_view(
            source(&BYTES, reads)?,
            PAGE_SIZE,
            0,
            PAGE_SIZE,
            0,
            false,
            super::super::PrivateMappingMode::CopyOnWrite,
        )?;
        let change = logical.prepare_map_private_view(
            logical.root_vmar(),
            range,
            view,
            Permissions::read_write(),
            Permissions::read_write(),
        )?;
        self.prepare_change(change)?.commit()?;
        Ok(())
    }
}

#[allow(
    dead_code,
    reason = "shared self-test entry; hardware coverage is architecture-dependent"
)]
pub(crate) fn prepare_native_entry_self_test(
    domain: ResourceDomain,
    range: UserSlice,
    code_range: UserSlice,
    stack_range: UserSlice,
    code: &[u8],
) -> Result<UniqueFallibleArc<NativeAddressSpace>, Error> {
    if code.is_empty() || code.len() > PAGE_SIZE as usize {
        return Err(Error::SizeOverflow);
    }
    let native = NativeAddressSpace::try_new(domain, range)?;
    let prepared = (|| {
        let reads = FallibleArc::try_new(AtomicU64::new(0)).map_err(|_| Error::Allocation)?;
        let logical = native.logical();
        let view = logical.prepare_file_view(
            source(code, reads)?,
            PAGE_SIZE,
            0,
            code.len() as u64,
            0,
            true,
            super::super::PrivateMappingMode::CopyOnWrite,
        )?;
        let code_change = logical.prepare_map_private_view(
            logical.root_vmar(),
            code_range,
            view,
            Permissions::read_execute(),
            Permissions::read_execute(),
        )?;
        native.prepare_change(code_change)?.commit()?;

        let stack = WritableVmo::try_new(PAGE_SIZE, KernelPageBackend, native.account.clone())?;
        stack
            .populate(0, PAGE_SIZE)
            .map_err(|failure| Error::Vmo(failure.cause))?;
        let stack_change = logical.prepare_map_writable(
            logical.root_vmar(),
            stack_range,
            stack,
            0,
            Permissions::read_write(),
            Permissions::read_write(),
        )?;
        native.prepare_change(stack_change)?.commit()?;
        // Every real userspace probe must fault on its first instruction.
        // Refill and instruction-cache publication use the production entry
        // path; no test-only trap handler repairs this deliberately absent page.
        require(native.reclaim_file_range(code_range)? == 1)
    })();
    match prepared {
        Ok(()) => Ok(native),
        Err(error) => {
            retire(native)?;
            Err(error)
        }
    }
}

fn retire(native: UniqueFallibleArc<NativeAddressSpace>) -> Result<(), Error> {
    NativeAddressSpace::retire_unpublished(native).map_err(|failure| {
        let (error, _owner) = failure.into_parts();
        error
    })
}

#[allow(
    dead_code,
    reason = "shared self-test entry; hardware coverage is architecture-dependent"
)]
pub(crate) fn run_dormant_self_test() -> Result<(), Error> {
    let domain = ResourceDomain::try_new_root(crate::accounting::ResourceLimits::UNLIMITED)?;
    let range = UserSlice::new(UserAddress::new(0x30_0000), PAGE_SIZE * 3)?;
    let native = NativeAddressSpace::try_new(domain.clone(), range)?;
    let result = check_pageable_roots(&native, &domain, range);
    retire(native)?;
    result
}

fn check_pageable_roots(
    native: &NativeAddressSpace,
    domain: &ResourceDomain,
    range: UserSlice,
) -> Result<(), Error> {
    let logical = native.logical();
    let reads = FallibleArc::try_new(AtomicU64::new(0)).map_err(|_| Error::Allocation)?;
    let data_range = UserSlice::new(range.base(), PAGE_SIZE * 2)?;
    let execute_range = UserSlice::new(
        UserAddress::new(range.base().get() + PAGE_SIZE * 2),
        PAGE_SIZE,
    )?;
    // Both the ordinary-data and executable preparations construct real HAL
    // page tables. The executable branch must publish instructions on initial
    // population and again after clean-page eviction.
    static BYTES: [u8; PAGE_SIZE as usize * 2] = [0x5a; PAGE_SIZE as usize * 2];
    for (mapping, executable) in [(data_range, false), (execute_range, true)] {
        let view = logical.prepare_file_view(
            source(&BYTES[..mapping.length() as usize], reads.clone())?,
            mapping.length(),
            0,
            mapping.length(),
            0,
            executable,
            super::super::PrivateMappingMode::CopyOnWrite,
        )?;
        let permissions = if executable {
            Permissions::read_execute()
        } else {
            Permissions::read_write()
        };
        let plan = logical.prepare_map_private_view(
            logical.root_vmar(),
            mapping,
            view,
            permissions,
            permissions,
        )?;
        native.prepare_change(plan)?.commit()?;
    }
    // Translation storage is charged to the same domain. Subtract only the
    // current root's owned table pages; a leaked retired root would still be
    // counted and fail these exact data-page checks.
    let pages = || {
        let tables = native
            .state
            .with(|state| state.current._tables.len() as u64);
        domain
            .usage()
            .committed(ResourceKind::CommittedPages)
            .saturating_sub(tables)
    };
    require(pages() == 3)?;
    let first = UserSlice::new(data_range.base(), 8)?;
    let mut bytes = [0; 8];
    native.copy_from_user(first, &mut bytes)?;
    require(bytes == [0x5a; 8])?;
    require(native.reclaim_file_range(range)? == 3)?;
    require(pages() == 0)?;
    require(matches!(
        logical.copy_from_user(first, &mut bytes),
        Err(LogicalError::BackingNotResident)
    ))?;

    let old_reads = reads.load(Ordering::Relaxed);
    let epoch = logical.mapping_epoch();
    require(native.resolve_file_fault(first.base(), Access::Read, epoch)?)?;
    require(pages() == 1 && reads.load(Ordering::Relaxed) > old_reads)?;
    native.copy_from_user(first, &mut bytes)?;
    require(bytes == [0x5a; 8])?;
    // A stale root may be retried once; a repeat with the now-current root
    // must not turn an unrelated protection fault into an infinite retry.
    require(native.resolve_file_fault(first.base(), Access::Read, epoch)?)?;
    require(!native.resolve_file_fault(first.base(), Access::Read, logical.mapping_epoch())?)?;
    require(!native.resolve_file_fault(first.base(), Access::Execute, logical.mapping_epoch())?)?;

    require(native.resolve_file_fault(first.base(), Access::Write, logical.mapping_epoch())?)?;
    native.copy_to_user(first, &[0xa5; 8])?;
    require(native.resolve_file_fault(
        execute_range.base(),
        Access::Execute,
        logical.mapping_epoch(),
    )?)?;
    require(pages() == 2)?;
    // Only the reloaded clean executable page is eligible: a dirty private
    // page cannot be recovered from its backing file without swap.
    require(native.reclaim_file_range(range)? == 1)?;
    require(pages() == 1)?;
    native.copy_from_user(first, &mut bytes)?;
    require(bytes == [0xa5; 8])?;
    require(native.resolve_file_fault(
        execute_range.base(),
        Access::Execute,
        logical.mapping_epoch(),
    )?)?;
    require(!native.resolve_file_fault(
        execute_range.base(),
        Access::Write,
        logical.mapping_epoch(),
    )?)?;
    native.copy_from_user(UserSlice::new(execute_range.base(), 8)?, &mut bytes)?;
    require(bytes == [0x5a; 8])?;
    crate::pr_info!("HypeR test: Native file pages reclaimed and refilled; dirty pages retained");
    Ok(())
}
