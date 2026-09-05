// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Native user-memory ownership and mapping transactions.

use std::boxed::Box;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Barrier, mpsc};
use std::time::Duration;

#[path = "../../../../src/kernel/mm/user_space/mod.rs"]
#[allow(dead_code, unused_imports)]
mod user;

use hyper::mm::{PAGE_SIZE, PhysicalAddress};
use user::{
    AddressSpaceError, ExecutableProvenance, MemoryAccount, MemoryCharge, PageBackend, Permissions,
    UserAddress, UserAddressSpace, UserAddressWindow, UserSlice, VmoError, WritableVmo,
};

#[derive(Default)]
struct Usage {
    bytes: AtomicU64,
    objects: AtomicU64,
    pages: AtomicU64,
    address_spaces: AtomicU64,
    mappings: AtomicU64,
    fail_next: AtomicBool,
    charge_calls: AtomicU64,
    fail_at_call: AtomicU64,
}

#[derive(Clone)]
struct Account(Arc<Usage>);

#[derive(Debug, Eq, PartialEq)]
struct AccountError;

struct Charge {
    usage: Arc<Usage>,
    amount: MemoryCharge,
}

impl Drop for Charge {
    fn drop(&mut self) {
        self.usage
            .bytes
            .fetch_sub(self.amount.kernel_bytes, Ordering::Relaxed);
        self.usage
            .objects
            .fetch_sub(self.amount.kernel_objects, Ordering::Relaxed);
        self.usage
            .pages
            .fetch_sub(self.amount.committed_pages, Ordering::Relaxed);
        self.usage
            .address_spaces
            .fetch_sub(self.amount.address_spaces, Ordering::Relaxed);
        self.usage
            .mappings
            .fetch_sub(self.amount.mappings, Ordering::Relaxed);
    }
}

impl MemoryAccount for Account {
    type Charge = Charge;
    type Error = AccountError;

    fn try_charge(&self, amount: MemoryCharge) -> Result<Self::Charge, Self::Error> {
        let call = self.0.charge_calls.fetch_add(1, Ordering::Relaxed) + 1;
        if self.0.fail_next.swap(false, Ordering::Relaxed)
            || self.0.fail_at_call.load(Ordering::Relaxed) == call
        {
            return Err(AccountError);
        }
        self.0
            .bytes
            .fetch_add(amount.kernel_bytes, Ordering::Relaxed);
        self.0
            .objects
            .fetch_add(amount.kernel_objects, Ordering::Relaxed);
        self.0
            .pages
            .fetch_add(amount.committed_pages, Ordering::Relaxed);
        self.0
            .address_spaces
            .fetch_add(amount.address_spaces, Ordering::Relaxed);
        self.0
            .mappings
            .fetch_add(amount.mappings, Ordering::Relaxed);
        Ok(Charge {
            usage: self.0.clone(),
            amount,
        })
    }
}

#[derive(Debug, Eq, PartialEq)]
struct PageError;

struct Page {
    physical: PhysicalAddress,
    bytes: Box<[u8; PAGE_SIZE as usize]>,
}

struct BackendState {
    next_physical: AtomicU64,
    allocation_calls: AtomicU64,
    read_calls: AtomicU64,
    write_calls: AtomicU64,
    exposed_write_calls: AtomicU64,
    fail_allocation_at: AtomicU64,
    fail_read_at: AtomicU64,
    fail_write_at: AtomicU64,
    block_next_read: AtomicBool,
    read_entered: Barrier,
    read_release: Barrier,
}

impl Default for BackendState {
    fn default() -> Self {
        Self {
            next_physical: AtomicU64::new(PAGE_SIZE),
            allocation_calls: AtomicU64::new(0),
            read_calls: AtomicU64::new(0),
            write_calls: AtomicU64::new(0),
            exposed_write_calls: AtomicU64::new(0),
            fail_allocation_at: AtomicU64::new(0),
            fail_read_at: AtomicU64::new(0),
            fail_write_at: AtomicU64::new(0),
            block_next_read: AtomicBool::new(false),
            read_entered: Barrier::new(2),
            read_release: Barrier::new(2),
        }
    }
}

#[derive(Clone)]
struct Backend(Arc<BackendState>);

impl PageBackend for Backend {
    type Page = Page;
    type Error = PageError;
    type InstructionPublicationContext = ();

    fn allocate_zeroed(&self) -> Result<Self::Page, Self::Error> {
        let call = self.0.allocation_calls.fetch_add(1, Ordering::Relaxed) + 1;
        if self.0.fail_allocation_at.load(Ordering::Relaxed) == call {
            return Err(PageError);
        }
        let physical = self.0.next_physical.fetch_add(PAGE_SIZE, Ordering::Relaxed);
        Ok(Page {
            physical: PhysicalAddress::new(physical),
            bytes: Box::new([0; PAGE_SIZE as usize]),
        })
    }

    fn physical_address(&self, page: &Self::Page) -> PhysicalAddress {
        page.physical
    }

    fn read_owned(
        &self,
        page: &Self::Page,
        offset: usize,
        destination: &mut [u8],
    ) -> Result<(), Self::Error> {
        if self.0.block_next_read.swap(false, Ordering::AcqRel) {
            self.0.read_entered.wait();
            self.0.read_release.wait();
        }
        let call = self.0.read_calls.fetch_add(1, Ordering::Relaxed) + 1;
        if self.0.fail_read_at.load(Ordering::Relaxed) == call {
            return Err(PageError);
        }
        let end = offset.checked_add(destination.len()).ok_or(PageError)?;
        let source = page.bytes.get(offset..end).ok_or(PageError)?;
        destination.copy_from_slice(source);
        Ok(())
    }

    fn write_owned(
        &self,
        page: &mut Self::Page,
        offset: usize,
        source: &[u8],
    ) -> Result<(), Self::Error> {
        let call = self.0.write_calls.fetch_add(1, Ordering::Relaxed) + 1;
        if self.0.fail_write_at.load(Ordering::Relaxed) == call {
            return Err(PageError);
        }
        let end = offset.checked_add(source.len()).ok_or(PageError)?;
        let destination = page.bytes.get_mut(offset..end).ok_or(PageError)?;
        destination.copy_from_slice(source);
        Ok(())
    }

    fn read_exposed(
        &self,
        page: &Self::Page,
        offset: usize,
        destination: &mut [u8],
    ) -> Result<(), Self::Error> {
        self.read_owned(page, offset, destination)
    }

    fn write_exposed(
        &self,
        page: &mut Self::Page,
        offset: usize,
        source: &[u8],
    ) -> Result<(), Self::Error> {
        self.0.exposed_write_calls.fetch_add(1, Ordering::Relaxed);
        self.write_owned(page, offset, source)
    }

    fn publish_instruction_pages(
        &self,
        _context: &Self::InstructionPublicationContext,
        mut pages: impl FnMut(&mut dyn FnMut(&mut Self::Page)),
    ) -> Result<(), Self::Error> {
        pages(&mut |_page| {});
        Ok(())
    }
}

fn fixtures() -> (Backend, Account) {
    (
        Backend(Arc::new(BackendState::default())),
        Account(Arc::new(Usage::default())),
    )
}

fn slice(base: u64, length: u64) -> UserSlice {
    crate::require_ok(UserSlice::new(UserAddress::new(base), length))
}

fn window() -> UserAddressWindow {
    crate::require_ok(UserAddressWindow::for_test(0, 1 << 32))
}

fn complete<BackendType: PageBackend, AccountType: MemoryAccount>(
    change: user::CommittedMappingChange<BackendType, AccountType>,
) {
    // SAFETY: Host tests install no hardware translation, so logical commit is
    // already quiescent on every CPU.
    unsafe { change.complete_retirement_for_test() };
}

#[test]
fn typed_ranges_check_overflow_and_empty_copy_is_a_noop() {
    assert!(UserSlice::new(UserAddress::new(u64::MAX), 1).is_err());
    let (backend, account) = fixtures();
    let address_space = crate::require_ok(UserAddressSpace::try_new(
        window(),
        slice(0x1000, PAGE_SIZE * 2),
        backend,
        account,
    ));
    let empty = crate::require_ok(UserSlice::new(UserAddress::new(u64::MAX), 0));
    assert!(address_space.copy_to_user(empty, &[]).is_ok());

    let (backend, account) = fixtures();
    assert!(
        UserAddressSpace::try_new(
            window(),
            slice((1u64 << 32) - PAGE_SIZE, PAGE_SIZE),
            backend.clone(),
            account.clone(),
        )
        .is_ok()
    );
    assert!(matches!(
        UserAddressSpace::try_new(
            window(),
            slice(1u64 << 32, PAGE_SIZE),
            backend.clone(),
            account.clone(),
        ),
        Err(AddressSpaceError::InvalidRange)
    ));
    let vmo = crate::require_ok(WritableVmo::try_new(PAGE_SIZE, backend, account));
    let error = match vmo.populate(1, PAGE_SIZE) {
        Ok(()) => panic!("unaligned population unexpectedly succeeded"),
        Err(error) => error,
    };
    assert_eq!(error.committed_pages, 0);
}

#[test]
fn map_copy_and_page_boundary_access_use_one_committed_epoch() {
    let (backend, account) = fixtures();
    let vmo = crate::require_ok(WritableVmo::try_new(
        PAGE_SIZE * 2,
        backend.clone(),
        account.clone(),
    ));
    assert!(vmo.populate(0, PAGE_SIZE * 2).is_ok());
    let address_space = crate::require_ok(UserAddressSpace::try_new(
        window(),
        slice(0x4000, PAGE_SIZE * 4),
        backend,
        account.clone(),
    ));
    let before = address_space.mapping_epoch();
    let prepared = crate::require_ok(address_space.prepare_map_writable(
        address_space.root_vmar(),
        slice(0x4000, PAGE_SIZE * 2),
        vmo,
        0,
        Permissions::read_write(),
        Permissions::read_write(),
    ));
    let token = crate::require_some(prepared.snapshots().next()).token;
    assert!(prepared.resident_pages(token).is_ok());
    let committed = crate::require_ok(prepared.commit_for_test());
    assert_eq!(committed.change().previous_epoch, before);
    assert_eq!(committed.change().epoch, before + 1);
    complete(committed);

    let bytes = [0x5a; 32];
    let range = slice(0x4000 + PAGE_SIZE - 16, 32);
    assert!(address_space.copy_to_user(range, &bytes).is_ok());
    let mut copied = [0; 32];
    assert!(address_space.copy_from_user(range, &mut copied).is_ok());
    assert_eq!(copied, bytes);
}

#[test]
fn partial_protect_and_unmap_split_records_and_invalidate_old_tokens() {
    let (backend, account) = fixtures();
    let vmo = crate::require_ok(WritableVmo::try_new(
        PAGE_SIZE * 3,
        backend.clone(),
        account.clone(),
    ));
    assert!(vmo.populate(0, PAGE_SIZE * 3).is_ok());
    let address_space = crate::require_ok(UserAddressSpace::try_new(
        window(),
        slice(0x10_000, PAGE_SIZE * 4),
        backend,
        account,
    ));
    let root = address_space.root_vmar();
    let map = crate::require_ok(address_space.prepare_map_writable(
        root,
        slice(0x10_000, PAGE_SIZE * 3),
        vmo,
        0,
        Permissions::read_write(),
        Permissions::read_write(),
    ));
    let old = crate::require_some(map.snapshots().next()).token;
    complete(crate::require_ok(map.commit_for_test()));

    let protect = crate::require_ok(address_space.prepare_protect(
        root,
        slice(0x11_000, PAGE_SIZE),
        Permissions::read_only(),
    ));
    assert_eq!(protect.snapshots().count(), 3);
    complete(crate::require_ok(protect.commit_for_test()));
    assert!(matches!(
        address_space.mapping_snapshot(old),
        Err(AddressSpaceError::StaleMapping)
    ));
    assert!(matches!(
        address_space.copy_to_user(slice(0x11_000, 1), &[1]),
        Err(AddressSpaceError::WriteDenied)
    ));

    let unmap = crate::require_ok(address_space.prepare_unmap(root, slice(0x11_000, PAGE_SIZE)));
    assert_eq!(unmap.snapshots().count(), 2);
    complete(crate::require_ok(unmap.commit_for_test()));
    let mut byte = [0];
    assert!(matches!(
        address_space.copy_from_user(slice(0x11_000, 1), &mut byte),
        Err(AddressSpaceError::NotMapped)
    ));
}

#[test]
fn executable_snapshot_is_distinct_and_wx_is_rejected() {
    let (backend, account) = fixtures();
    let writable = crate::require_ok(WritableVmo::try_new(
        PAGE_SIZE,
        backend.clone(),
        account.clone(),
    ));
    assert!(writable.write(0, &[1, 2, 3]).is_ok());
    let executable =
        crate::require_ok(writable.try_executable_snapshot(&ExecutableProvenance::for_test(), &()));
    assert!(writable.write(0, &[9, 9, 9]).is_ok());
    let mut bytes = [0; 3];
    assert!(executable.read(0, &mut bytes).is_ok());
    assert_eq!(bytes, [1, 2, 3]);

    let address_space = crate::require_ok(UserAddressSpace::try_new(
        window(),
        slice(0x20_000, PAGE_SIZE * 2),
        backend,
        account,
    ));
    assert!(matches!(
        address_space.prepare_map_executable(
            address_space.root_vmar(),
            slice(0x20_000, PAGE_SIZE),
            executable,
            0,
            Permissions::read_write(),
            Permissions::read_write(),
        ),
        Err(AddressSpaceError::WritableExecutableBacking)
    ));
}

#[test]
fn nested_vmar_reservations_prevent_parent_bypass() {
    let (backend, account) = fixtures();
    let address_space = crate::require_ok(UserAddressSpace::try_new(
        window(),
        slice(0x30_000, PAGE_SIZE * 8),
        backend.clone(),
        account.clone(),
    ));
    let root = address_space.root_vmar();
    let child =
        crate::require_ok(address_space.try_create_vmar(root, slice(0x32_000, PAGE_SIZE * 4)));
    assert!(
        address_space
            .try_create_vmar(child, slice(0x33_000, PAGE_SIZE))
            .is_ok()
    );
    let vmo = crate::require_ok(WritableVmo::try_new(PAGE_SIZE, backend, account));
    assert!(vmo.populate(0, PAGE_SIZE).is_ok());
    assert!(matches!(
        address_space.prepare_map_writable(
            root,
            slice(0x32_000, PAGE_SIZE),
            vmo,
            0,
            Permissions::read_only(),
            Permissions::read_write(),
        ),
        Err(AddressSpaceError::Overlap)
    ));
}

#[test]
fn vmar_authority_changes_do_not_advance_machine_mapping_epoch() {
    let (backend, account) = fixtures();
    let address_space = crate::require_ok(UserAddressSpace::try_new(
        window(),
        slice(0x38_000, PAGE_SIZE * 4),
        backend.clone(),
        account.clone(),
    ));
    let root = address_space.root_vmar();
    let machine_epoch = address_space.mapping_epoch();
    let child =
        crate::require_ok(address_space.try_create_vmar(root, slice(0x39_000, PAGE_SIZE * 2)));
    assert_eq!(address_space.mapping_epoch(), machine_epoch);

    let vmo = crate::require_ok(WritableVmo::try_new(PAGE_SIZE, backend, account));
    assert!(vmo.populate(0, PAGE_SIZE).is_ok());
    let prepared = crate::require_ok(address_space.prepare_map_writable(
        child,
        slice(0x39_000, PAGE_SIZE),
        vmo,
        0,
        Permissions::read_only(),
        Permissions::read_write(),
    ));
    let committed = crate::require_ok(prepared.commit_for_test());
    assert_eq!(committed.change().previous_epoch, machine_epoch);
    assert_eq!(committed.change().epoch, machine_epoch + 1);
    complete(committed);
}

#[test]
fn account_failure_leaves_mapping_epoch_and_authority_unchanged() {
    let (backend, account) = fixtures();
    let address_space = crate::require_ok(UserAddressSpace::try_new(
        window(),
        slice(0x40_000, PAGE_SIZE * 2),
        backend.clone(),
        account.clone(),
    ));
    let vmo = crate::require_ok(WritableVmo::try_new(PAGE_SIZE, backend, account.clone()));
    assert!(vmo.populate(0, PAGE_SIZE).is_ok());
    let epoch = address_space.mapping_epoch();
    account.0.fail_next.store(true, Ordering::Relaxed);
    assert!(matches!(
        address_space.prepare_map_writable(
            address_space.root_vmar(),
            slice(0x40_000, PAGE_SIZE),
            vmo,
            0,
            Permissions::read_only(),
            Permissions::read_write(),
        ),
        Err(AddressSpaceError::Account(AccountError))
    ));
    assert_eq!(address_space.mapping_epoch(), epoch);
    let mut byte = [0];
    assert!(matches!(
        address_space.copy_from_user(slice(0x40_000, 1), &mut byte),
        Err(AddressSpaceError::NotMapped)
    ));
}

#[test]
fn partial_population_reports_only_pages_won_by_this_call() {
    let (backend, account) = fixtures();
    let vmo = crate::require_ok(WritableVmo::try_new(
        PAGE_SIZE * 2,
        backend,
        account.clone(),
    ));
    let next = account.0.charge_calls.load(Ordering::Relaxed) + 2;
    account.0.fail_at_call.store(next, Ordering::Relaxed);
    let error = match vmo.populate(0, PAGE_SIZE * 2) {
        Ok(()) => panic!("partial population unexpectedly succeeded"),
        Err(error) => error,
    };
    assert_eq!(error.committed_pages, 1);
    assert_eq!(account.0.pages.load(Ordering::Relaxed), 1);

    let (backend, account) = fixtures();
    let vmo = crate::require_ok(WritableVmo::try_new(
        PAGE_SIZE * 2,
        backend.clone(),
        account.clone(),
    ));
    backend.0.fail_allocation_at.store(2, Ordering::Relaxed);
    let error = match vmo.populate(0, PAGE_SIZE * 2) {
        Ok(()) => panic!("backend population failure unexpectedly succeeded"),
        Err(error) => error,
    };
    assert_eq!(error.committed_pages, 1);
    assert_eq!(account.0.pages.load(Ordering::Relaxed), 1);
}

#[test]
fn competing_prepared_changes_allow_only_one_epoch_commit() {
    let (backend, account) = fixtures();
    let address_space = crate::require_ok(UserAddressSpace::try_new(
        window(),
        slice(0x50_000, PAGE_SIZE * 4),
        backend.clone(),
        account.clone(),
    ));
    let first_vmo = crate::require_ok(WritableVmo::try_new(
        PAGE_SIZE,
        backend.clone(),
        account.clone(),
    ));
    let usage = account.0.clone();
    let second_vmo = crate::require_ok(WritableVmo::try_new(
        PAGE_SIZE,
        backend.clone(),
        account.clone(),
    ));
    assert!(first_vmo.populate(0, PAGE_SIZE).is_ok());
    assert!(second_vmo.populate(0, PAGE_SIZE).is_ok());
    let first = crate::require_ok(address_space.prepare_map_writable(
        address_space.root_vmar(),
        slice(0x50_000, PAGE_SIZE),
        first_vmo,
        0,
        Permissions::read_only(),
        Permissions::read_write(),
    ));
    let second = crate::require_ok(address_space.prepare_map_writable(
        address_space.root_vmar(),
        slice(0x51_000, PAGE_SIZE),
        second_vmo,
        0,
        Permissions::read_only(),
        Permissions::read_write(),
    ));
    let losing_token = crate::require_some(second.snapshots().last()).token;
    let losing_pages = crate::require_ok(second.resident_pages(losing_token));
    complete(crate::require_ok(first.commit_for_test()));
    assert!(matches!(
        second.commit_for_test(),
        Err(AddressSpaceError::StaleTransaction)
    ));
    assert!(matches!(
        address_space.mapping_snapshot(losing_token),
        Err(AddressSpaceError::StaleMapping)
    ));
    assert_eq!(usage.pages.load(Ordering::Relaxed), 2);
    assert_eq!(usage.mappings.load(Ordering::Relaxed), 2);
    assert_eq!(losing_pages.pages().len(), 1);
    drop(losing_pages);
    assert_eq!(usage.pages.load(Ordering::Relaxed), 1);
    assert_eq!(usage.mappings.load(Ordering::Relaxed), 1);

    let third_vmo = crate::require_ok(WritableVmo::try_new(PAGE_SIZE, backend, account));
    assert!(third_vmo.populate(0, PAGE_SIZE).is_ok());
    let third = crate::require_ok(address_space.prepare_map_writable(
        address_space.root_vmar(),
        slice(0x52_000, PAGE_SIZE),
        third_vmo,
        0,
        Permissions::read_only(),
        Permissions::read_only(),
    ));
    let replacement_token = crate::require_some(third.snapshots().last()).token;
    assert_ne!(replacement_token, losing_token);
    complete(crate::require_ok(third.commit_for_test()));
    assert!(address_space.mapping_snapshot(replacement_token).is_ok());
    assert!(matches!(
        address_space.mapping_snapshot(losing_token),
        Err(AddressSpaceError::StaleMapping)
    ));
}

#[test]
fn retired_mapping_pins_backing_until_quiescence_acknowledgement() {
    let (backend, account) = fixtures();
    let usage = account.0.clone();
    let address_space = crate::require_ok(UserAddressSpace::try_new(
        window(),
        slice(0x60_000, PAGE_SIZE * 2),
        backend.clone(),
        account.clone(),
    ));
    let vmo = crate::require_ok(WritableVmo::try_new(PAGE_SIZE, backend, account));
    assert!(vmo.populate(0, PAGE_SIZE).is_ok());
    let map = crate::require_ok(address_space.prepare_map_writable(
        address_space.root_vmar(),
        slice(0x60_000, PAGE_SIZE),
        vmo,
        0,
        Permissions::read_only(),
        Permissions::read_write(),
    ));
    complete(crate::require_ok(map.commit_for_test()));
    assert_eq!(usage.pages.load(Ordering::Relaxed), 1);
    assert_eq!(usage.mappings.load(Ordering::Relaxed), 1);

    let unmap = crate::require_ok(
        address_space.prepare_unmap(address_space.root_vmar(), slice(0x60_000, PAGE_SIZE)),
    );
    let retired = crate::require_ok(unmap.commit_for_test());
    let before_acknowledgement = usage.bytes.load(Ordering::Relaxed);
    assert_eq!(usage.pages.load(Ordering::Relaxed), 1);
    assert_eq!(usage.mappings.load(Ordering::Relaxed), 1);
    complete(retired);
    assert_eq!(usage.pages.load(Ordering::Relaxed), 0);
    assert_eq!(usage.mappings.load(Ordering::Relaxed), 0);
    assert!(usage.bytes.load(Ordering::Relaxed) < before_acknowledgement);
}

#[test]
fn dropping_unacknowledged_retirement_safely_pins_old_ownership() {
    let (backend, account) = fixtures();
    let usage = account.0.clone();
    {
        let address_space = crate::require_ok(UserAddressSpace::try_new(
            window(),
            slice(0x68_000, PAGE_SIZE * 2),
            backend.clone(),
            account.clone(),
        ));
        let vmo = crate::require_ok(WritableVmo::try_new(PAGE_SIZE, backend, account));
        assert!(vmo.populate(0, PAGE_SIZE).is_ok());
        let map = crate::require_ok(address_space.prepare_map_writable(
            address_space.root_vmar(),
            slice(0x68_000, PAGE_SIZE),
            vmo,
            0,
            Permissions::read_only(),
            Permissions::read_only(),
        ));
        complete(crate::require_ok(map.commit_for_test()));
        let unmap = crate::require_ok(
            address_space.prepare_unmap(address_space.root_vmar(), slice(0x68_000, PAGE_SIZE)),
        );
        drop(crate::require_ok(unmap.commit_for_test()));
    }
    assert_eq!(usage.pages.load(Ordering::Relaxed), 1);
    assert_eq!(usage.mappings.load(Ordering::Relaxed), 1);
    assert!(usage.objects.load(Ordering::Relaxed) >= 1);
}

#[test]
fn copy_plan_survives_concurrent_unmap_without_holding_address_space_lock() {
    let (backend, account) = fixtures();
    let address_space = Arc::new(crate::require_ok(UserAddressSpace::try_new(
        window(),
        slice(0x70_000, PAGE_SIZE * 2),
        backend.clone(),
        account.clone(),
    )));
    let vmo = crate::require_ok(WritableVmo::try_new(PAGE_SIZE, backend.clone(), account));
    assert!(vmo.populate(0, PAGE_SIZE).is_ok());
    assert!(vmo.write(0, &[0x7a]).is_ok());
    let direct = vmo.clone();
    let map = crate::require_ok(address_space.prepare_map_writable(
        address_space.root_vmar(),
        slice(0x70_000, PAGE_SIZE),
        vmo,
        0,
        Permissions::read_write(),
        Permissions::read_write(),
    ));
    complete(crate::require_ok(map.commit_for_test()));

    backend.0.block_next_read.store(true, Ordering::Release);
    let copy_space = address_space.clone();
    let copy_thread = std::thread::spawn(move || {
        let mut byte = [0];
        let result = copy_space.copy_from_user(slice(0x70_000, 1), &mut byte);
        (result, byte)
    });
    backend.0.read_entered.wait();

    let unmap_space = address_space.clone();
    let root = address_space.root_vmar();
    let (sender, receiver) = mpsc::channel();
    let unmap_thread = std::thread::spawn(move || {
        let result = unmap_space
            .prepare_unmap(root, slice(0x70_000, PAGE_SIZE))
            .and_then(|prepared| prepared.commit_for_test())
            .map(complete);
        let _ = sender.send(result.is_ok());
    });
    let committed_while_copy_blocked = receiver.recv_timeout(Duration::from_secs(1));
    let mut direct_byte = [0];
    assert!(matches!(
        direct.read(0, &mut direct_byte),
        Err(VmoError::Busy)
    ));
    backend.0.read_release.wait();

    let (copy_result, copied) = match copy_thread.join() {
        Ok(result) => result,
        Err(_) => panic!("copy thread panicked"),
    };
    if unmap_thread.join().is_err() {
        panic!("unmap thread panicked");
    }
    assert_eq!(committed_while_copy_blocked, Ok(true));
    assert!(copy_result.is_ok());
    assert_eq!(copied, [0x7a]);
    assert!(direct.read(0, &mut direct_byte).is_ok());
    assert_eq!(direct_byte, [0x7a]);
    let mut byte = [0];
    assert!(matches!(
        address_space.copy_from_user(slice(0x70_000, 1), &mut byte),
        Err(AddressSpaceError::NotMapped)
    ));
}

#[test]
fn user_write_reservation_blocks_mapping_commit_until_release() {
    let (backend, account) = fixtures();
    let address_space = crate::require_ok(UserAddressSpace::try_new(
        window(),
        slice(0x72_000, PAGE_SIZE),
        backend.clone(),
        account.clone(),
    ));
    let vmo = crate::require_ok(WritableVmo::try_new(PAGE_SIZE, backend, account));
    assert!(vmo.populate(0, PAGE_SIZE).is_ok());
    let map = crate::require_ok(address_space.prepare_map_writable(
        address_space.root_vmar(),
        slice(0x72_000, PAGE_SIZE),
        vmo,
        0,
        Permissions::read_write(),
        Permissions::read_write(),
    ));
    complete(crate::require_ok(map.commit_for_test()));

    let first_reservation =
        crate::require_ok(address_space.prepare_user_write_for_test(slice(0x72_000, 1)));
    let second_reservation =
        crate::require_ok(address_space.prepare_user_write_for_test(slice(0x72_000, 1)));
    let unmap = crate::require_ok(
        address_space.prepare_unmap(address_space.root_vmar(), slice(0x72_000, PAGE_SIZE)),
    );
    assert!(matches!(
        unmap.commit_for_test(),
        Err(AddressSpaceError::Busy)
    ));
    address_space.release_user_write_for_test(first_reservation);
    let unmap = crate::require_ok(
        address_space.prepare_unmap(address_space.root_vmar(), slice(0x72_000, PAGE_SIZE)),
    );
    assert!(matches!(
        unmap.commit_for_test(),
        Err(AddressSpaceError::Busy)
    ));
    assert!(
        address_space
            .write_user_reservation_for_test(&second_reservation, &[0xa5])
            .is_ok()
    );
    address_space.release_user_write_for_test(second_reservation);

    let mut observed = [0];
    assert!(
        address_space
            .copy_from_user(slice(0x72_000, 1), &mut observed)
            .is_ok()
    );
    assert_eq!(observed, [0xa5]);
    let unmap = crate::require_ok(
        address_space.prepare_unmap(address_space.root_vmar(), slice(0x72_000, PAGE_SIZE)),
    );
    complete(crate::require_ok(unmap.commit_for_test()));
}

#[test]
fn backend_failures_report_defined_partial_copy_effects() {
    let (backend, account) = fixtures();
    let address_space = crate::require_ok(UserAddressSpace::try_new(
        window(),
        slice(0x80_000, PAGE_SIZE * 3),
        backend.clone(),
        account.clone(),
    ));
    let first = crate::require_ok(WritableVmo::try_new(
        PAGE_SIZE,
        backend.clone(),
        account.clone(),
    ));
    let second = crate::require_ok(WritableVmo::try_new(PAGE_SIZE, backend.clone(), account));
    assert!(first.populate(0, PAGE_SIZE).is_ok());
    assert!(second.populate(0, PAGE_SIZE).is_ok());
    let first_map = crate::require_ok(address_space.prepare_map_writable(
        address_space.root_vmar(),
        slice(0x80_000, PAGE_SIZE),
        first.clone(),
        0,
        Permissions::read_write(),
        Permissions::read_write(),
    ));
    complete(crate::require_ok(first_map.commit_for_test()));
    let second_map = crate::require_ok(address_space.prepare_map_writable(
        address_space.root_vmar(),
        slice(0x81_000, PAGE_SIZE),
        second.clone(),
        0,
        Permissions::read_write(),
        Permissions::read_write(),
    ));
    complete(crate::require_ok(second_map.commit_for_test()));

    assert!(
        address_space
            .copy_to_user(slice(0x80_000 + PAGE_SIZE - 1, 2), &[1, 2])
            .is_ok()
    );
    let mut adjacent = [0; 2];
    assert!(
        address_space
            .copy_from_user(slice(0x80_000 + PAGE_SIZE - 1, 2), &mut adjacent)
            .is_ok()
    );
    assert_eq!(adjacent, [1, 2]);

    let next_write = backend.0.write_calls.load(Ordering::Relaxed) + 2;
    backend.0.fail_write_at.store(next_write, Ordering::Relaxed);
    assert!(matches!(
        address_space.copy_to_user(slice(0x80_000 + PAGE_SIZE - 1, 2), &[3, 4]),
        Err(AddressSpaceError::Backend(PageError))
    ));
    backend.0.fail_write_at.store(0, Ordering::Relaxed);
    let mut first_byte = [0];
    let mut second_byte = [0];
    assert!(
        address_space
            .copy_from_user(slice(0x80_000 + PAGE_SIZE - 1, 1), &mut first_byte)
            .is_ok()
    );
    assert!(
        address_space
            .copy_from_user(slice(0x81_000, 1), &mut second_byte)
            .is_ok()
    );
    assert_eq!(first_byte, [3]);
    assert_eq!(second_byte, [2]);

    let next_read = backend.0.read_calls.load(Ordering::Relaxed) + 2;
    backend.0.fail_read_at.store(next_read, Ordering::Relaxed);
    let mut destination = [0xcc; 2];
    assert!(matches!(
        address_space.copy_from_user(slice(0x80_000 + PAGE_SIZE - 1, 2), &mut destination,),
        Err(AddressSpaceError::Backend(PageError))
    ));
    assert_eq!(destination, [3, 0xcc]);
}

#[test]
fn permission_ceiling_allows_only_explicit_monotonic_authority() {
    let (backend, account) = fixtures();
    let address_space = crate::require_ok(UserAddressSpace::try_new(
        window(),
        slice(0x90_000, PAGE_SIZE * 3),
        backend.clone(),
        account.clone(),
    ));
    let root = address_space.root_vmar();
    let first = crate::require_ok(WritableVmo::try_new(
        PAGE_SIZE,
        backend.clone(),
        account.clone(),
    ));
    assert!(first.populate(0, PAGE_SIZE).is_ok());
    let map = crate::require_ok(address_space.prepare_map_writable(
        root,
        slice(0x90_000, PAGE_SIZE),
        first,
        0,
        Permissions::NONE,
        Permissions::read_write(),
    ));
    complete(crate::require_ok(map.commit_for_test()));
    let mut byte = [0];
    assert!(matches!(
        address_space.copy_from_user(slice(0x90_000, 1), &mut byte),
        Err(AddressSpaceError::ReadDenied)
    ));
    assert!(matches!(
        address_space.copy_to_user(slice(0x90_000, 1), &[1]),
        Err(AddressSpaceError::WriteDenied)
    ));
    let enable = crate::require_ok(address_space.prepare_protect(
        root,
        slice(0x90_000, PAGE_SIZE),
        Permissions::read_write(),
    ));
    complete(crate::require_ok(enable.commit_for_test()));
    assert!(address_space.copy_to_user(slice(0x90_000, 1), &[9]).is_ok());
    let disable = crate::require_ok(address_space.prepare_protect(
        root,
        slice(0x90_000, PAGE_SIZE),
        Permissions::NONE,
    ));
    complete(crate::require_ok(disable.commit_for_test()));
    let reenable = crate::require_ok(address_space.prepare_protect(
        root,
        slice(0x90_000, PAGE_SIZE),
        Permissions::read_write(),
    ));
    complete(crate::require_ok(reenable.commit_for_test()));

    let second = crate::require_ok(WritableVmo::try_new(PAGE_SIZE, backend, account));
    assert!(second.populate(0, PAGE_SIZE).is_ok());
    let map = crate::require_ok(address_space.prepare_map_writable(
        root,
        slice(0x91_000, PAGE_SIZE),
        second,
        0,
        Permissions::read_only(),
        Permissions::read_only(),
    ));
    complete(crate::require_ok(map.commit_for_test()));
    assert!(matches!(
        address_space.prepare_protect(root, slice(0x91_000, PAGE_SIZE), Permissions::read_write(),),
        Err(AddressSpaceError::InvalidPermissions)
    ));
}

#[test]
fn executable_snapshot_waits_for_retired_writable_translation_authority() {
    let (backend, account) = fixtures();
    let address_space = crate::require_ok(UserAddressSpace::try_new(
        window(),
        slice(0xd0_000, PAGE_SIZE * 2),
        backend.clone(),
        account.clone(),
    ));
    let writable = crate::require_ok(WritableVmo::try_new(PAGE_SIZE, backend, account));
    assert!(writable.populate(0, PAGE_SIZE).is_ok());
    let snapshot_source = writable.clone();
    let map = crate::require_ok(address_space.prepare_map_writable(
        address_space.root_vmar(),
        slice(0xd0_000, PAGE_SIZE),
        writable,
        0,
        Permissions::read_write(),
        Permissions::read_write(),
    ));
    complete(crate::require_ok(map.commit_for_test()));

    assert!(matches!(
        snapshot_source.try_executable_snapshot(&ExecutableProvenance::for_test(), &()),
        Err(VmoError::Busy)
    ));
    let protect = crate::require_ok(address_space.prepare_protect(
        address_space.root_vmar(),
        slice(0xd0_000, PAGE_SIZE),
        Permissions::read_only(),
    ));
    let retired_writable = crate::require_ok(protect.commit_for_test());
    assert!(matches!(
        snapshot_source.try_executable_snapshot(&ExecutableProvenance::for_test(), &()),
        Err(VmoError::Busy)
    ));

    complete(retired_writable);
    assert!(
        snapshot_source
            .try_executable_snapshot(&ExecutableProvenance::for_test(), &())
            .is_ok()
    );
}

#[test]
fn abandoned_or_stale_write_upgrade_releases_snapshot_admission() {
    let (backend, account) = fixtures();
    let address_space = crate::require_ok(UserAddressSpace::try_new(
        window(),
        slice(0xe0_000, PAGE_SIZE * 3),
        backend.clone(),
        account.clone(),
    ));
    let writable = crate::require_ok(WritableVmo::try_new(
        PAGE_SIZE,
        backend.clone(),
        account.clone(),
    ));
    assert!(writable.populate(0, PAGE_SIZE).is_ok());
    let snapshot_source = writable.clone();
    let map = crate::require_ok(address_space.prepare_map_writable(
        address_space.root_vmar(),
        slice(0xe0_000, PAGE_SIZE),
        writable,
        0,
        Permissions::read_only(),
        Permissions::read_write(),
    ));
    complete(crate::require_ok(map.commit_for_test()));

    let abandoned = crate::require_ok(address_space.prepare_protect(
        address_space.root_vmar(),
        slice(0xe0_000, PAGE_SIZE),
        Permissions::read_write(),
    ));
    assert!(matches!(
        snapshot_source.try_executable_snapshot(&ExecutableProvenance::for_test(), &()),
        Err(VmoError::Busy)
    ));
    drop(abandoned);
    assert!(
        snapshot_source
            .try_executable_snapshot(&ExecutableProvenance::for_test(), &())
            .is_ok()
    );

    let stale = crate::require_ok(address_space.prepare_protect(
        address_space.root_vmar(),
        slice(0xe0_000, PAGE_SIZE),
        Permissions::read_write(),
    ));
    let other = crate::require_ok(WritableVmo::try_new(PAGE_SIZE, backend, account));
    assert!(other.populate(0, PAGE_SIZE).is_ok());
    let competing = crate::require_ok(address_space.prepare_map_writable(
        address_space.root_vmar(),
        slice(0xe1_000, PAGE_SIZE),
        other,
        0,
        Permissions::read_only(),
        Permissions::read_only(),
    ));
    complete(crate::require_ok(competing.commit_for_test()));
    assert!(matches!(
        stale.commit_for_test(),
        Err(AddressSpaceError::StaleTransaction)
    ));
    assert!(
        snapshot_source
            .try_executable_snapshot(&ExecutableProvenance::for_test(), &())
            .is_ok()
    );
}

#[test]
fn direct_vmo_access_is_excluded_by_active_and_retiring_write_mappings() {
    let (backend, account) = fixtures();
    let address_space = crate::require_ok(UserAddressSpace::try_new(
        window(),
        slice(0xf0_000, PAGE_SIZE * 2),
        backend.clone(),
        account.clone(),
    ));
    let writable = crate::require_ok(WritableVmo::try_new(PAGE_SIZE, backend, account));
    assert!(writable.populate(0, PAGE_SIZE).is_ok());
    let direct = writable.clone();
    let map = crate::require_ok(address_space.prepare_map_writable(
        address_space.root_vmar(),
        slice(0xf0_000, PAGE_SIZE),
        writable,
        0,
        Permissions::read_write(),
        Permissions::read_write(),
    ));
    complete(crate::require_ok(map.commit_for_test()));

    let mut byte = [0];
    assert!(matches!(direct.read(0, &mut byte), Err(VmoError::Busy)));
    assert!(matches!(direct.write(0, &[1]), Err(VmoError::Busy)));
    assert!(address_space.copy_to_user(slice(0xf0_000, 1), &[7]).is_ok());
    assert!(
        address_space
            .copy_from_user(slice(0xf0_000, 1), &mut byte)
            .is_ok()
    );
    assert_eq!(byte, [7]);

    let unmap = crate::require_ok(
        address_space.prepare_unmap(address_space.root_vmar(), slice(0xf0_000, PAGE_SIZE)),
    );
    let retired = crate::require_ok(unmap.commit_for_test());
    assert!(matches!(direct.read(0, &mut byte), Err(VmoError::Busy)));
    complete(retired);
    assert!(direct.read(0, &mut byte).is_ok());
    assert_eq!(byte, [7]);
    assert!(direct.write(0, &[9]).is_ok());
}

#[test]
fn direct_write_uses_exposed_access_with_a_read_only_machine_mapping() {
    let (backend, account) = fixtures();
    let address_space = crate::require_ok(UserAddressSpace::try_new(
        window(),
        slice(0xf2_000, PAGE_SIZE),
        backend.clone(),
        account.clone(),
    ));
    let writable = crate::require_ok(WritableVmo::try_new(PAGE_SIZE, backend.clone(), account));
    assert!(writable.populate(0, PAGE_SIZE).is_ok());
    let direct = writable.clone();
    let map = crate::require_ok(address_space.prepare_map_writable(
        address_space.root_vmar(),
        slice(0xf2_000, PAGE_SIZE),
        writable,
        0,
        Permissions::read_only(),
        Permissions::read_only(),
    ));
    complete(crate::require_ok(map.commit_for_test()));

    assert!(direct.write(0, &[0x5a]).is_ok());
    assert_eq!(backend.0.exposed_write_calls.load(Ordering::Relaxed), 1);
    let mut observed = [0];
    assert!(
        address_space
            .copy_from_user(slice(0xf2_000, 1), &mut observed)
            .is_ok()
    );
    assert_eq!(observed, [0x5a]);
}

#[test]
fn vmar_tokens_enforce_hierarchy_lifetime_and_address_space_identity() {
    let (backend, account) = fixtures();
    let address_space = crate::require_ok(UserAddressSpace::try_new(
        window(),
        slice(0xa0_000, PAGE_SIZE * 8),
        backend.clone(),
        account.clone(),
    ));
    let root = address_space.root_vmar();
    let child =
        crate::require_ok(address_space.try_create_vmar(root, slice(0xa2_000, PAGE_SIZE * 4)));
    assert!(matches!(
        address_space.try_create_vmar(root, slice(0xa1_000, PAGE_SIZE * 2)),
        Err(AddressSpaceError::Overlap)
    ));
    let grandchild =
        crate::require_ok(address_space.try_create_vmar(child, slice(0xa3_000, PAGE_SIZE)));
    assert!(matches!(
        address_space.destroy_vmar(child),
        Err(AddressSpaceError::InvalidRange)
    ));
    assert!(address_space.destroy_vmar(grandchild).is_ok());

    let other = crate::require_ok(UserAddressSpace::try_new(
        window(),
        slice(0xb0_000, PAGE_SIZE * 2),
        backend.clone(),
        account.clone(),
    ));
    assert!(matches!(
        address_space.try_create_vmar(other.root_vmar(), slice(0xa2_000, PAGE_SIZE),),
        Err(AddressSpaceError::InvalidAddressSpace)
    ));

    let vmo = crate::require_ok(WritableVmo::try_new(PAGE_SIZE, backend, account));
    assert!(vmo.populate(0, PAGE_SIZE).is_ok());
    let map = crate::require_ok(address_space.prepare_map_writable(
        child,
        slice(0xa2_000, PAGE_SIZE),
        vmo,
        0,
        Permissions::read_only(),
        Permissions::read_only(),
    ));
    complete(crate::require_ok(map.commit_for_test()));
    assert!(matches!(
        address_space.prepare_unmap(root, slice(0xa2_000, PAGE_SIZE)),
        Err(AddressSpaceError::NotMapped)
    ));
    assert!(matches!(
        address_space.destroy_vmar(child),
        Err(AddressSpaceError::InvalidRange)
    ));
    let unmap = crate::require_ok(address_space.prepare_unmap(child, slice(0xa2_000, PAGE_SIZE)));
    complete(crate::require_ok(unmap.commit_for_test()));
    assert!(address_space.destroy_vmar(child).is_ok());
    assert!(matches!(
        address_space.try_create_vmar(child, slice(0xa2_000, PAGE_SIZE)),
        Err(AddressSpaceError::StaleVmar)
    ));
}

#[test]
fn copy_prevalidation_rejects_gaps_before_mutating_any_mapping() {
    let (backend, account) = fixtures();
    let address_space = crate::require_ok(UserAddressSpace::try_new(
        window(),
        slice(0xc0_000, PAGE_SIZE * 4),
        backend.clone(),
        account.clone(),
    ));
    let first = crate::require_ok(WritableVmo::try_new(
        PAGE_SIZE,
        backend.clone(),
        account.clone(),
    ));
    let second = crate::require_ok(WritableVmo::try_new(PAGE_SIZE, backend, account));
    assert!(first.populate(0, PAGE_SIZE).is_ok());
    assert!(second.populate(0, PAGE_SIZE).is_ok());
    let first_map = crate::require_ok(address_space.prepare_map_writable(
        address_space.root_vmar(),
        slice(0xc0_000, PAGE_SIZE),
        first.clone(),
        0,
        Permissions::read_write(),
        Permissions::read_write(),
    ));
    complete(crate::require_ok(first_map.commit_for_test()));
    let second_map = crate::require_ok(address_space.prepare_map_writable(
        address_space.root_vmar(),
        slice(0xc2_000, PAGE_SIZE),
        second,
        0,
        Permissions::read_write(),
        Permissions::read_write(),
    ));
    complete(crate::require_ok(second_map.commit_for_test()));

    let source = std::vec![0x5a; PAGE_SIZE as usize + 2];
    assert!(matches!(
        address_space.copy_to_user(slice(0xc0_000 + PAGE_SIZE - 1, PAGE_SIZE + 2), &source,),
        Err(AddressSpaceError::NotMapped)
    ));
    let mut last_byte = [0xff];
    assert!(
        address_space
            .copy_from_user(slice(0xc0_000 + PAGE_SIZE - 1, 1), &mut last_byte)
            .is_ok()
    );
    assert_eq!(last_byte, [0]);
}
