// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Sparse physically owned VM objects and immutable executable snapshots.

use alloc::vec::Vec;
use core::mem::size_of;
use core::sync::atomic::{AtomicUsize, Ordering};

use hyper::mm::{AllocationError, FallibleArc, PAGE_SIZE, PhysicalAddress};
use hyper::sync::InterruptSpinLock;

use super::contract::{MemoryAccount, MemoryCharge, PageBackend};

mod direct_copy;
mod file_source;
mod private_view;
pub(super) use direct_copy::PinnedReadRange;
use file_source::FileBacking;
pub(crate) use file_source::FilePageSource;
pub(super) use private_view::{PinnedPrivateRange, PrivateView, PrivateViewBuilder};

#[cfg(not(test))]
pub(super) type UserMemoryLock<T> = InterruptSpinLock<T, crate::hal::irq::LocalMask>;

#[cfg(test)]
pub(super) struct TestInterruptMask;

#[cfg(test)]
impl hyper::hal::interrupt::InterruptMask for TestInterruptMask {
    type State = ();

    fn save_and_disable() -> Self::State {}
    fn restore(_: Self::State) {}
    fn wait_for_lock_owner() {
        std::thread::yield_now();
    }
}

#[cfg(test)]
pub(super) type UserMemoryLock<T> = InterruptSpinLock<T, TestInterruptMask>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum VmoError<BackendError, AccountError> {
    Account(AccountError),
    Allocation,
    Backend(BackendError),
    Busy,
    CowRequired,
    BackingNotResident,
    BackingIo,
    InvalidRange,
    SizeOverflow,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct VmoPopulateError<BackendError, AccountError> {
    pub(crate) cause: VmoError<BackendError, AccountError>,
    pub(crate) committed_pages: usize,
}

struct PagePreparationError<BackendError, AccountError> {
    cause: VmoError<BackendError, AccountError>,
    committed_pages: usize,
}

struct OwnedPage<Page, Charge> {
    page: Page,
    _charge: Charge,
}

type PageRef<Backend, Account> = FallibleArc<
    UserMemoryLock<OwnedPage<<Backend as PageBackend>::Page, <Account as MemoryAccount>::Charge>>,
>;

struct VmoState<Backend: PageBackend, Account: MemoryAccount> {
    pages: Vec<Option<PageRef<Backend, Account>>>,
    resident_pages: usize,
    guest_loaned: bool,
}

type VmoLock<Backend, Account> = UserMemoryLock<VmoState<Backend, Account>>;
type VmoResult<Backend, Account, Value> =
    Result<Value, VmoError<<Backend as PageBackend>::Error, <Account as MemoryAccount>::Error>>;

struct VmoInner<Backend: PageBackend, Account: MemoryAccount> {
    size: u64,
    backend: Backend,
    account: Account,
    state: VmoLock<Backend, Account>,
    access_state: AtomicUsize,
    _metadata_charge: Account::Charge,
}

pub(crate) struct WritableVmo<Backend: PageBackend, Account: MemoryAccount> {
    inner: FallibleArc<VmoInner<Backend, Account>>,
}

pub(crate) struct ExecutableVmo<Backend: PageBackend, Account: MemoryAccount> {
    inner: FallibleArc<VmoInner<Backend, Account>>,
}

/// Immutable, non-executable contents with independently counted physical owners.
pub(crate) struct SnapshotVmo<Backend: PageBackend, Account: MemoryAccount> {
    inner: FallibleArc<VmoInner<Backend, Account>>,
}

impl<B: PageBackend, A: MemoryAccount> Clone for SnapshotVmo<B, A> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<B: PageBackend, A: MemoryAccount> SnapshotVmo<B, A> {
    pub(crate) fn try_from_bytes(bytes: &[u8], backend: B, account: A) -> VmoResult<B, A, Self> {
        let size = u64::try_from(bytes.len()).map_err(|_| VmoError::SizeOverflow)?;
        let size = size
            .max(1)
            .checked_add(PAGE_SIZE - 1)
            .ok_or(VmoError::SizeOverflow)?
            / PAGE_SIZE
            * PAGE_SIZE;
        let writable = WritableVmo::try_new(size, backend, account)?;
        writable.populate(0, size).map_err(|error| error.cause)?;
        writable.write(0, bytes)?;
        // The local writable owner has never escaped or acquired a mapping.
        Ok(Self {
            inner: writable.inner,
        })
    }

    #[cfg(test)]
    pub(crate) fn resident_physical_page(&self, offset: u64) -> VmoResult<B, A, PhysicalAddress> {
        if !offset.is_multiple_of(PAGE_SIZE) {
            return Err(VmoError::InvalidRange);
        }
        let page = page_ref(
            &self.inner,
            usize::try_from(offset / PAGE_SIZE).map_err(|_| VmoError::SizeOverflow)?,
        )?;
        Ok(page.with(|owned| self.inner.backend.physical_address(&owned.page)))
    }

    pub(crate) fn size(&self) -> u64 {
        self.inner.size
    }
    pub(crate) fn read(&self, offset: u64, bytes: &mut [u8]) -> VmoResult<B, A, ()> {
        read_owned_inner(&self.inner, offset, bytes)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PrivateMappingMode {
    CopyOnWrite,
    Eager,
}

fn own_shared_page<B: PageBackend, A: MemoryAccount>(
    page: B::Page,
    account: &A,
) -> VmoResult<B, A, PageRef<B, A>> {
    // A mapping retains physical storage even when the cache would prefer to
    // evict it. Charge the requester conservatively, not the filesystem owner.
    let charge = account
        .try_charge(MemoryCharge {
            kernel_bytes:
                FallibleArc::<UserMemoryLock<OwnedPage<B::Page, A::Charge>>>::allocation_size()
                    as u64,
            committed_pages: 1,
            ..MemoryCharge::default()
        })
        .map_err(VmoError::Account)?;
    FallibleArc::try_new(UserMemoryLock::new(OwnedPage {
        page,
        _charge: charge,
    }))
    .map_err(map_allocation)
}

fn allocate_page<B: PageBackend, A: MemoryAccount>(
    backend: &B,
    account: &A,
) -> VmoResult<B, A, PageRef<B, A>> {
    let charge = account
        .try_charge(MemoryCharge {
            kernel_bytes:
                FallibleArc::<UserMemoryLock<OwnedPage<B::Page, A::Charge>>>::allocation_size()
                    as u64,
            committed_pages: 1,
            ..MemoryCharge::default()
        })
        .map_err(VmoError::Account)?;
    let page = backend.allocate_zeroed().map_err(VmoError::Backend)?;
    FallibleArc::try_new(UserMemoryLock::new(OwnedPage {
        page,
        _charge: charge,
    }))
    .map_err(map_allocation)
}

struct WritableMappingLeaseInner<Backend: PageBackend, Account: MemoryAccount> {
    vmo: FallibleArc<VmoInner<Backend, Account>>,
    _charge: Account::Charge,
}

pub(crate) struct WritableMappingLease<Backend: PageBackend, Account: MemoryAccount> {
    inner: FallibleArc<WritableMappingLeaseInner<Backend, Account>>,
}

struct ExclusiveHardwareWriteLeaseInner<Backend: PageBackend, Account: MemoryAccount> {
    vmo: FallibleArc<VmoInner<Backend, Account>>,
    _charge: Account::Charge,
}

/// Persistent hardware-write ownership excluding Native writers and snapshots.
///
/// Acquisition rejects writable Native mappings, direct kernel access, and
/// snapshots. While this lease exists, those operations remain closed;
/// read-only Native mappings may coexist. This is the quiescence proof required
/// before publishing executable bytes and exposing pages through guest translations.
/// Explicit guest-memory capabilities may clone the same admission; Native
/// access reopens only after every authorized hardware owner retires.
pub(crate) struct ExclusiveHardwareWriteLease<Backend: PageBackend, Account: MemoryAccount> {
    inner: FallibleArc<ExclusiveHardwareWriteLeaseInner<Backend, Account>>,
}

impl<B: PageBackend, A: MemoryAccount> Clone for ExclusiveHardwareWriteLease<B, A> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

/// Opaque authority to publish bytes as native executable provenance.
pub(crate) struct ExecutableProvenance {
    _private: (),
}

#[cfg(test)]
impl ExecutableProvenance {
    pub(crate) const fn for_test() -> Self {
        Self { _private: () }
    }
}

impl ExecutableProvenance {
    pub(crate) const fn for_native_image_loader() -> Self {
        Self { _private: () }
    }

    pub(super) const fn for_capability() -> Self {
        Self { _private: () }
    }
}

impl<Backend: PageBackend, Account: MemoryAccount> Clone for WritableVmo<Backend, Account> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<Backend: PageBackend, Account: MemoryAccount> Clone for ExecutableVmo<Backend, Account> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<Backend: PageBackend, Account: MemoryAccount> Clone
    for WritableMappingLease<Backend, Account>
{
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<Backend: PageBackend, Account: MemoryAccount> WritableMappingLease<Backend, Account> {
    #[cfg(any(test, all(feature = "kernel-test", feature = "kernel-self-test")))]
    pub(crate) fn write_exposed(
        &self,
        offset: u64,
        source: &[u8],
    ) -> VmoResult<Backend, Account, ()> {
        self.exposed_copy_target().write_exposed(offset, source)
    }

    /// Selects the exact backing whose externally writable lifetime this lease
    /// retains. Copies through it use external-memory primitives; the lease
    /// excludes snapshots and exclusive hardware ownership, not peer writes.
    pub(super) fn exposed_copy_target(&self) -> WritableVmo<Backend, Account> {
        WritableVmo {
            inner: self.inner.vmo.clone(),
        }
    }
}

impl<Backend: PageBackend, Account: MemoryAccount> WritableVmo<Backend, Account> {
    /// Imports already-zeroed, already-accounted owned pages before publication.
    /// This constructor charges VMO/page metadata; the supplied page owners
    /// retain their physical-allocation charge through the final reference.
    pub(crate) fn try_from_owned_pages(
        size: u64,
        backend: Backend,
        account: Account,
        pages: Vec<Backend::Page>,
    ) -> VmoResult<Backend, Account, Self> {
        if page_count(size)? != pages.len() {
            return Err(VmoError::InvalidRange);
        }
        let vmo = Self::try_new(size, backend, account)?;
        for (index, page) in pages.into_iter().enumerate() {
            let charge = vmo
                .inner
                .account
                .try_charge(MemoryCharge {
                    kernel_bytes: FallibleArc::<
                        UserMemoryLock<OwnedPage<Backend::Page, Account::Charge>>,
                    >::allocation_size() as u64,
                    ..MemoryCharge::default()
                })
                .map_err(VmoError::Account)?;
            let owner = FallibleArc::try_new(UserMemoryLock::new(OwnedPage {
                page,
                _charge: charge,
            }))
            .map_err(map_allocation)?;
            // Local construction owns every slot; no allocation or destructor
            // runs under the page table lock and the VMO has not escaped.
            vmo.inner.state.with(|state| {
                state.pages[index] = Some(owner);
                state.resident_pages += 1;
            });
        }
        Ok(vmo)
    }

    pub(crate) fn try_new(
        size: u64,
        backend: Backend,
        account: Account,
    ) -> Result<Self, VmoError<Backend::Error, Account::Error>> {
        let page_count = page_count(size)?;
        let metadata = metadata_charge::<Backend, Account>(page_count)?;
        let metadata_charge = account.try_charge(metadata).map_err(VmoError::Account)?;
        let mut pages = Vec::new();
        pages
            .try_reserve_exact(page_count)
            .map_err(|_| VmoError::Allocation)?;
        pages.resize_with(page_count, || None);
        let inner = VmoInner {
            size,
            backend,
            account,
            state: UserMemoryLock::new(VmoState {
                pages,
                resident_pages: 0,
                guest_loaned: false,
            }),
            access_state: AtomicUsize::new(0),
            _metadata_charge: metadata_charge,
        };
        Ok(Self {
            inner: FallibleArc::try_new(inner).map_err(map_allocation)?,
        })
    }

    /// Exact vector backing required before a writable VMO can be published.
    /// Platform admission/reclaim policy remains with the production caller.
    pub(crate) fn metadata_layout(
        size: u64,
    ) -> Result<core::alloc::Layout, VmoError<Backend::Error, Account::Error>> {
        core::alloc::Layout::array::<Option<PageRef<Backend, Account>>>(page_count(size)?)
            .map_err(|_| VmoError::SizeOverflow)
    }

    pub(crate) fn size(&self) -> u64 {
        self.inner.size
    }

    /// Acquires persistent authority for a hardware-writable mapping.
    ///
    /// The lease remains live through address-space retirement, so executable
    /// snapshot admission cannot race a stale writable translation.
    pub(crate) fn try_mapping_write_lease(
        &self,
    ) -> VmoResult<Backend, Account, WritableMappingLease<Backend, Account>> {
        let admission = MappingAdmission::acquire(&self.inner)?;
        let charge = self
            .inner
            .account
            .try_charge(MemoryCharge {
                kernel_bytes:
                    FallibleArc::<WritableMappingLeaseInner<Backend, Account>>::allocation_size()
                        as u64,
                kernel_objects: 1,
                ..MemoryCharge::default()
            })
            .map_err(VmoError::Account)?;
        let lease_inner = WritableMappingLeaseInner {
            vmo: self.inner.clone(),
            _charge: charge,
        };
        // Ownership of the admission moves to `lease_inner` before the
        // fallible allocation. FallibleArc drops its input on allocation
        // failure, so exactly one owner releases the admission on every path.
        admission.commit();
        let inner = FallibleArc::try_new(lease_inner).map_err(map_allocation)?;
        Ok(WritableMappingLease { inner })
    }

    /// Acquires the sole hardware-write lease for this VMO.
    pub(crate) fn try_exclusive_hardware_write_lease(
        &self,
    ) -> VmoResult<Backend, Account, ExclusiveHardwareWriteLease<Backend, Account>> {
        let admission = ExclusiveHardwareAdmission::acquire(&self.inner)?;
        let charge =
            self.inner
                .account
                .try_charge(
                    MemoryCharge {
                        kernel_bytes: FallibleArc::<
                            ExclusiveHardwareWriteLeaseInner<Backend, Account>,
                        >::allocation_size() as u64,
                        kernel_objects: 1,
                        ..MemoryCharge::default()
                    },
                )
                .map_err(VmoError::Account)?;
        let lease_inner = ExclusiveHardwareWriteLeaseInner {
            vmo: self.inner.clone(),
            _charge: charge,
        };
        // The allocation owns admission release even when allocation fails.
        admission.commit();
        let inner = FallibleArc::try_new(lease_inner).map_err(map_allocation)?;
        // Only the successfully allocated lease starts accounting. Population
        // can run concurrently: both publication and this transition hold the
        // VMO state lock, so existing and future pages are each counted once.
        self.inner.state.with(|state| {
            self.inner.backend.retain_guest_pages(state.resident_pages);
            state.guest_loaned = true;
        });
        Ok(ExclusiveHardwareWriteLease { inner })
    }

    pub(crate) fn read(
        &self,
        offset: u64,
        destination: &mut [u8],
    ) -> Result<(), VmoError<Backend::Error, Account::Error>> {
        let access = KernelAccessGuard::acquire(&self.inner)?;
        let result = read_owned_inner(&self.inner, offset, destination);
        drop(access);
        result
    }

    /// Explicitly commits stable physical backing for a page-aligned range.
    ///
    /// Failure reports how many pages this call committed before the failing
    /// allocation. Those pages remain valid VMO ownership; callers must not
    /// mistake this operation for a rollbackable mapping preparation.
    pub(crate) fn populate(
        &self,
        offset: u64,
        length: u64,
    ) -> Result<(), VmoPopulateError<Backend::Error, Account::Error>> {
        if length == 0 || !offset.is_multiple_of(PAGE_SIZE) || !length.is_multiple_of(PAGE_SIZE) {
            return Err(VmoPopulateError {
                cause: VmoError::InvalidRange,
                committed_pages: 0,
            });
        }
        let length_usize = usize::try_from(length).map_err(|_| VmoPopulateError {
            cause: VmoError::SizeOverflow,
            committed_pages: 0,
        })?;
        validate_range(self.inner.size, offset, length_usize).map_err(|cause| {
            VmoPopulateError {
                cause,
                committed_pages: 0,
            }
        })?;
        let (first, count) =
            covered_pages(offset, length_usize).map_err(|cause| VmoPopulateError {
                cause,
                committed_pages: 0,
            })?;
        self.prepare_pages(first, count)
            .map(|_| ())
            .map_err(|failure| VmoPopulateError {
                cause: failure.cause,
                committed_pages: failure.committed_pages,
            })
    }

    pub(crate) fn same_storage(&self, other: &Self) -> bool {
        core::ptr::eq(&*self.inner, &*other.inner)
    }

    /// Admits a resident kernel-owned write before any page is changed. The
    /// owner must use exposed copies because read-only machine aliases can
    /// remain, but snapshots and guest/writable aliases must be excluded.
    pub(super) fn begin_kernel_write(
        &self,
    ) -> VmoResult<Backend, Account, KernelAccessGuard<'_, Backend, Account>> {
        KernelAccessGuard::acquire(&self.inner)
    }

    /// Counts retained backing without populating pages or cloning page owners.
    /// Bounded lock holds keep an inspection from delaying page publication.
    pub(crate) fn resident_bytes(
        &self,
        offset: u64,
        length: u64,
    ) -> VmoResult<Backend, Account, u64> {
        let length = usize::try_from(length).map_err(|_| VmoError::SizeOverflow)?;
        validate_range(self.size(), offset, length)?;
        let (first, count) = covered_pages(offset, length)?;
        let end = first.checked_add(count).ok_or(VmoError::SizeOverflow)?;
        let mut resident = 0;
        for start in (first..end).step_by(256) {
            let limit = end.min(start.saturating_add(256));
            resident += self.inner.state.with(|state| {
                state.pages[start..limit]
                    .iter()
                    .filter(|page| page.is_some())
                    .count()
            });
        }
        Ok(resident as u64 * PAGE_SIZE)
    }

    /// Returns the stable physical page backing one resident object offset.
    ///
    /// The caller must retain this VMO, and any hardware mapping which may
    /// write the page must additionally retain an
    /// [`ExclusiveHardwareWriteLease`].
    /// The returned address remains stable because resident pages are never
    /// removed before the final VMO owner is destroyed.
    pub(crate) fn resident_physical_page(
        &self,
        offset: u64,
    ) -> Result<PhysicalAddress, VmoError<Backend::Error, Account::Error>> {
        if !offset.is_multiple_of(PAGE_SIZE) || offset >= self.inner.size {
            return Err(VmoError::InvalidRange);
        }
        let index = usize::try_from(offset / PAGE_SIZE).map_err(|_| VmoError::SizeOverflow)?;
        let page = page_ref(&self.inner, index)?;
        Ok(page.with(|owned| self.inner.backend.physical_address(&owned.page)))
    }

    /// Reads a resident range which may be concurrently visible to a machine.
    pub(crate) fn read_exposed(
        &self,
        offset: u64,
        destination: &mut [u8],
    ) -> Result<(), VmoError<Backend::Error, Account::Error>> {
        read_exposed_inner(&self.inner, offset, destination)
    }

    /// Writes a resident range which may be concurrently visible to a machine.
    pub(crate) fn write_exposed(
        &self,
        offset: u64,
        source: &[u8],
    ) -> Result<(), VmoError<Backend::Error, Account::Error>> {
        write_exposed_inner(&self.inner, offset, source)
    }

    pub(crate) fn page_is_resident(
        &self,
        offset: u64,
    ) -> Result<bool, VmoError<Backend::Error, Account::Error>> {
        if !offset.is_multiple_of(PAGE_SIZE) || offset >= self.inner.size {
            return Err(VmoError::InvalidRange);
        }
        let index = usize::try_from(offset / PAGE_SIZE).map_err(|_| VmoError::SizeOverflow)?;
        Ok(optional_page_ref(&self.inner, index)?.is_some())
    }

    /// Ensures every page in the range has stable physical backing.
    ///
    /// Each page is admitted and published independently, keeping IRQ-masked
    /// metadata sections constant-time. A later allocation failure may leave
    /// an earlier page validly committed to this VMO, but never publishes a
    /// partial address-space mapping.
    fn prepare_pages(
        &self,
        first: usize,
        count: usize,
    ) -> Result<usize, PagePreparationError<Backend::Error, Account::Error>> {
        let end = first.checked_add(count).ok_or(PagePreparationError {
            cause: VmoError::SizeOverflow,
            committed_pages: 0,
        })?;
        let mut committed_pages = 0;
        for index in first..end {
            let present = self
                .inner
                .state
                .with(|state| {
                    state
                        .pages
                        .get(index)
                        .map(Option::is_some)
                        .ok_or(VmoError::InvalidRange)
                })
                .map_err(|cause| PagePreparationError {
                    cause,
                    committed_pages,
                })?;
            if present {
                continue;
            }
            let charge = self
                .inner
                .account
                .try_charge(MemoryCharge {
                    kernel_bytes: FallibleArc::<
                        UserMemoryLock<OwnedPage<Backend::Page, Account::Charge>>,
                    >::allocation_size() as u64,
                    committed_pages: 1,
                    ..MemoryCharge::default()
                })
                .map_err(|error| PagePreparationError {
                    cause: VmoError::Account(error),
                    committed_pages,
                })?;
            let page =
                self.inner
                    .backend
                    .allocate_zeroed()
                    .map_err(|error| PagePreparationError {
                        cause: VmoError::Backend(error),
                        committed_pages,
                    })?;
            let page = FallibleArc::try_new(UserMemoryLock::new(OwnedPage {
                page,
                _charge: charge,
            }))
            .map_err(|error| PagePreparationError {
                cause: map_allocation(error),
                committed_pages,
            })?;
            let loser = self.inner.state.with(|state| {
                let slot = match state.pages.get_mut(index) {
                    Some(slot) => slot,
                    None => return Some(page),
                };
                if slot.is_none() {
                    *slot = Some(page);
                    state.resident_pages += 1;
                    if state.guest_loaned {
                        self.inner.backend.retain_guest_pages(1);
                    }
                    None
                } else {
                    Some(page)
                }
            });
            // Page/accounting destruction is deliberately outside the VMO
            // lock when another first-touch won publication.
            let installed = loser.is_none();
            drop(loser);
            if installed {
                committed_pages += 1;
            }
        }
        Ok(committed_pages)
    }

    /// Writes object bytes while excluding snapshots and writable mappings.
    ///
    /// A read-only machine mapping may appear or remain active concurrently,
    /// so this path deliberately uses the backend's exposed-memory primitive.
    /// The operation may leave an earlier prefix modified on backend failure.
    pub(crate) fn write(
        &self,
        offset: u64,
        source: &[u8],
    ) -> Result<(), VmoError<Backend::Error, Account::Error>> {
        validate_range(self.inner.size, offset, source.len())?;
        let access = KernelAccessGuard::acquire(&self.inner)?;
        let (first, count) = covered_pages(offset, source.len())?;
        self.prepare_pages(first, count)
            .map_err(|failure| failure.cause)?;
        visit_chunks(
            offset,
            source.len(),
            |index, page_offset, source_offset, length| {
                let page = page_ref(&self.inner, index)?;
                page.with(|owned| {
                    self.inner
                        .backend
                        .write_exposed(
                            &mut owned.page,
                            page_offset,
                            &source[source_offset..source_offset + length],
                        )
                        .map_err(VmoError::Backend)
                })
            },
        )?;
        drop(access);
        Ok(())
    }

    pub(crate) fn try_snapshot(
        &self,
        account: Account,
    ) -> VmoResult<Backend, Account, SnapshotVmo<Backend, Account>> {
        let _freeze = SnapshotGuard::acquire(&self.inner)?;
        let snapshot = WritableVmo::try_new(self.size(), self.inner.backend.clone(), account)?;
        snapshot
            .populate(0, self.size())
            .map_err(|error| error.cause)?;
        self.copy_frozen_pages_to(&snapshot)?;
        Ok(SnapshotVmo {
            inner: snapshot.inner,
        })
    }

    /// Produces a coherent, fully backed, physically distinct image.
    ///
    /// Snapshot admission excludes new writers and waits for no context: it
    /// reports `Busy` while a writer is active. The returned type exposes no
    /// write operation, and no source page is shared with executable backing.
    #[cfg_attr(
        not(any(test, all(feature = "kernel-test", feature = "kernel-self-test"))),
        expect(
            dead_code,
            reason = "executable snapshot conversion currently exercised by memory self-tests"
        )
    )]
    pub(crate) fn try_executable_snapshot(
        &self,
        _provenance: &ExecutableProvenance,
        publication_context: &Backend::InstructionPublicationContext,
    ) -> Result<ExecutableVmo<Backend, Account>, VmoError<Backend::Error, Account::Error>> {
        let freeze = SnapshotGuard::acquire(&self.inner)?;
        let snapshot = WritableVmo::try_new(
            self.inner.size,
            self.inner.backend.clone(),
            self.inner.account.clone(),
        )?;
        let page_count = page_count(self.inner.size)?;
        snapshot
            .prepare_pages(0, page_count)
            .map_err(|failure| failure.cause)?;
        self.copy_frozen_pages_to(&snapshot)?;
        drop(freeze);
        snapshot.publish_instruction_pages(publication_context)?;
        Ok(ExecutableVmo {
            inner: snapshot.inner,
        })
    }

    /// Caller holds the source snapshot guard and exclusively owns a fully
    /// populated, zeroed destination which has never been published. VMO
    /// construction requires a page-aligned size, so every copy is a full page.
    fn copy_frozen_pages_to(&self, destination: &Self) -> VmoResult<Backend, Account, ()> {
        for index in 0..page_count(self.size())? {
            let Some(source) = optional_page_ref(&self.inner, index)? else {
                // Sparse source bytes are zero; the destination already is.
                continue;
            };
            let target = page_ref(&destination.inner, index)?;
            // Only this transaction can reach target. Its lock under source
            // cannot form a cycle. Failure discards every unpublished target.
            source.with(|owned| {
                target.with(|target| {
                    self.inner
                        .backend
                        .copy_owned(&owned.page, 0, &mut target.page, 0, PAGE_SIZE as usize)
                        .map_err(VmoError::Backend)
                })
            })?;
        }
        Ok(())
    }

    fn publish_instruction_pages(
        &self,
        publication_context: &Backend::InstructionPublicationContext,
    ) -> Result<(), VmoError<Backend::Error, Account::Error>> {
        let count = page_count(self.inner.size)?;
        let mut missing = false;
        self.inner
            .backend
            .publish_instruction_pages(publication_context, |visit| {
                for index in 0..count {
                    let Some(page) = optional_page_ref(&self.inner, index).ok().flatten() else {
                        missing = true;
                        continue;
                    };
                    page.with(|owned| visit(&mut owned.page));
                }
            })
            .map_err(VmoError::Backend)?;
        if missing {
            return Err(VmoError::InvalidRange);
        }
        Ok(())
    }
}

impl<Backend: PageBackend, Account: MemoryAccount> ExecutableVmo<Backend, Account> {
    pub(crate) fn snapshot(&self) -> SnapshotVmo<Backend, Account> {
        SnapshotVmo {
            inner: self.inner.clone(),
        }
    }
    pub(crate) fn size(&self) -> u64 {
        self.inner.size
    }

    pub(crate) fn read(
        &self,
        offset: u64,
        destination: &mut [u8],
    ) -> Result<(), VmoError<Backend::Error, Account::Error>> {
        read_owned_inner(&self.inner, offset, destination)
    }
}

impl<Backend: PageBackend, Account: MemoryAccount> Drop
    for WritableMappingLeaseInner<Backend, Account>
{
    fn drop(&mut self) {
        release_mapping_admission(&self.vmo);
    }
}

impl<Backend: PageBackend, Account: MemoryAccount> Drop
    for ExclusiveHardwareWriteLeaseInner<Backend, Account>
{
    fn drop(&mut self) {
        self.vmo.state.with(|state| {
            if state.guest_loaned {
                self.vmo.backend.release_guest_pages(state.resident_pages);
                state.guest_loaned = false;
            }
        });
        release_exclusive_hardware_admission(&self.vmo);
    }
}

/// A private atomic-word pin keeps exactly its physical frame alive. It does
/// not keep removed pages in an older private-view version resident.
pub(super) struct PinnedPrivatePage<B: PageBackend, A: MemoryAccount> {
    _owner: PageRef<B, A>,
}

/// One resident page retained without its VMO, mappings, file source or locks.
pub(super) struct CrashPage<B: PageBackend, A: MemoryAccount> {
    owner: PageRef<B, A>,
    backend: B,
}

impl<B: PageBackend, A: MemoryAccount> CrashPage<B, A> {
    fn try_new(owner: PageRef<B, A>, backend: B) -> Option<Self> {
        let eligible = owner.with(|owned| backend.permits_crash_retention(&owned.page));
        eligible.then_some(Self { owner, backend })
    }

    pub(super) fn read(&self, offset: usize, output: &mut [u8]) -> Result<(), B::Error> {
        self.owner
            .with(|owned| self.backend.read_exposed(&owned.page, offset, output))
    }
}

pub(super) enum MappingObject<Backend: PageBackend, Account: MemoryAccount> {
    Snapshot(SnapshotVmo<Backend, Account>),
    Private(FallibleArc<PrivateView<Backend, Account>>),
    Writable(WritableVmo<Backend, Account>),
    Executable(ExecutableVmo<Backend, Account>),
}

impl<Backend: PageBackend, Account: MemoryAccount> Clone for MappingObject<Backend, Account> {
    fn clone(&self) -> Self {
        match self {
            Self::Snapshot(vmo) => Self::Snapshot(vmo.clone()),
            Self::Private(view) => Self::Private(view.clone()),
            Self::Writable(vmo) => Self::Writable(vmo.clone()),
            Self::Executable(vmo) => Self::Executable(vmo.clone()),
        }
    }
}

impl<Backend: PageBackend, Account: MemoryAccount> MappingObject<Backend, Account> {
    /// The stopped address space has closed mutation admission and owns the
    /// only mapping. `VmoInner` never publishes weak references. Therefore these
    /// two known owners (the mapping and its lease) exclude exported VMOs,
    /// another machine mapping, DMA leases and in-flight VMO/copy operations.
    /// A diagnostic refcount load alone would not establish that boundary.
    /// An old foreign VMAR preparation may still clone a mapping snapshot, but
    /// it cannot write backing and its final commit is rejected after sealing.
    /// Target copies/faults must be quiesced before this observation; the count
    /// itself is not a lock or proof that borrowed references have disappeared.
    pub(super) fn crash_writable_is_private(
        &self,
        lease: Option<&WritableMappingLease<Backend, Account>>,
    ) -> bool {
        match (self, lease) {
            (Self::Writable(vmo), Some(lease)) => vmo.inner.is_only_owner_pair(&lease.inner.vmo),
            _ => false,
        }
    }

    pub(super) fn retain_crash_page(
        &self,
        offset: u64,
        private_writable: bool,
    ) -> Option<CrashPage<Backend, Account>> {
        if !offset.is_multiple_of(PAGE_SIZE) {
            return None;
        }
        let index = usize::try_from(offset / PAGE_SIZE).ok()?;
        match self {
            Self::Private(view) => view.retain_crash_page(index),
            Self::Writable(vmo) if private_writable => {
                let owner = vmo
                    .inner
                    .state
                    .with(|state| state.pages.get(index).cloned().flatten())?;
                CrashPage::try_new(owner, vmo.inner.backend.clone())
            }
            _ => None,
        }
    }

    pub(super) fn pin_private_page(
        &self,
        offset: u64,
    ) -> VmoResult<Backend, Account, Option<PinnedPrivatePage<Backend, Account>>> {
        let Self::Private(view) = self else {
            return Ok(None);
        };
        let index = usize::try_from(offset / PAGE_SIZE).map_err(|_| VmoError::SizeOverflow)?;
        let page = view
            .pages
            .get(index)
            .and_then(Option::as_ref)
            .ok_or(VmoError::InvalidRange)?;
        if !view.is_writable(index) {
            return Err(VmoError::CowRequired);
        }
        Ok(Some(PinnedPrivatePage {
            _owner: page.owner.clone(),
        }))
    }

    pub(super) fn private(&self) -> Option<&FallibleArc<PrivateView<Backend, Account>>> {
        match self {
            Self::Private(view) => Some(view),
            _ => None,
        }
    }
    pub(super) fn writable_backing(
        &self,
        offset: u64,
        length: usize,
    ) -> VmoResult<Backend, Account, bool> {
        match self {
            Self::Writable(_) => Ok(true),
            Self::Private(view) => view.range_writable(offset, length),
            _ => Ok(false),
        }
    }
    pub(super) fn physical_page(
        &self,
        offset: u64,
    ) -> VmoResult<Backend, Account, PhysicalAddress> {
        if !offset.is_multiple_of(PAGE_SIZE) {
            return Err(VmoError::InvalidRange);
        }
        let index = usize::try_from(offset / PAGE_SIZE).map_err(|_| VmoError::SizeOverflow)?;
        match self {
            Self::Private(view) => {
                let page = view
                    .pages
                    .get(index)
                    .and_then(Option::as_ref)
                    .ok_or(VmoError::BackingNotResident)?;
                Ok(page
                    .owner
                    .with(|owned| view.backend.physical_address(&owned.page)))
            }
            Self::Writable(vmo) => vmo.resident_physical_page(offset),
            Self::Snapshot(vmo) => {
                let page = page_ref(&vmo.inner, index)?;
                Ok(page.with(|owned| vmo.inner.backend.physical_address(&owned.page)))
            }
            Self::Executable(vmo) => {
                let page = page_ref(&vmo.inner, index)?;
                Ok(page.with(|owned| vmo.inner.backend.physical_address(&owned.page)))
            }
        }
    }
    pub(super) fn size(&self) -> u64 {
        match self {
            Self::Snapshot(vmo) => vmo.size(),
            Self::Private(view) => view.size(),
            Self::Writable(vmo) => vmo.size(),
            Self::Executable(vmo) => vmo.size(),
        }
    }

    pub(super) fn executable(&self) -> bool {
        matches!(self, Self::Executable(_))
            || matches!(self, Self::Private(view) if view.executable)
    }

    pub(super) fn try_write_lease(
        &self,
    ) -> VmoResult<Backend, Account, WritableMappingLease<Backend, Account>> {
        match self {
            Self::Writable(vmo) => vmo.try_mapping_write_lease(),
            Self::Executable(_) | Self::Snapshot(_) | Self::Private(_) => {
                Err(VmoError::InvalidRange)
            }
        }
    }

    pub(super) fn fully_resident(
        &self,
        offset: u64,
        length: u64,
    ) -> Result<bool, VmoError<Backend::Error, Account::Error>> {
        let length = usize::try_from(length).map_err(|_| VmoError::SizeOverflow)?;
        let (first, count) = covered_pages(offset, length)?;
        let inner = match self {
            Self::Private(view) => {
                return Ok(offset
                    .checked_add(length as u64)
                    .is_some_and(|end| end <= view.size()));
            }
            Self::Snapshot(vmo) => &vmo.inner,
            Self::Writable(vmo) => &vmo.inner,
            Self::Executable(vmo) => &vmo.inner,
        };
        Ok(resident_count(inner, first, count)? == count)
    }

    pub(super) fn read_exposed(
        &self,
        offset: u64,
        destination: &mut [u8],
    ) -> Result<(), VmoError<Backend::Error, Account::Error>> {
        match self {
            Self::Snapshot(vmo) => read_exposed_inner(&vmo.inner, offset, destination),
            Self::Private(view) => view.read(offset, destination),
            Self::Writable(vmo) => read_exposed_inner(&vmo.inner, offset, destination),
            Self::Executable(vmo) => read_exposed_inner(&vmo.inner, offset, destination),
        }
    }

    pub(super) fn write_exposed(
        &self,
        offset: u64,
        source: &[u8],
    ) -> Result<(), VmoError<Backend::Error, Account::Error>> {
        match self {
            Self::Private(view) => view.write(offset, source),
            Self::Snapshot(_) => Err(VmoError::InvalidRange),
            Self::Writable(vmo) => write_exposed_inner(&vmo.inner, offset, source),
            Self::Executable(_) => Err(VmoError::InvalidRange),
        }
    }
}

pub(super) struct KernelAccessGuard<'a, Backend: PageBackend, Account: MemoryAccount> {
    inner: &'a VmoInner<Backend, Account>,
}

struct MappingAdmission<'a, Backend: PageBackend, Account: MemoryAccount> {
    inner: &'a VmoInner<Backend, Account>,
    armed: bool,
}

struct ExclusiveHardwareAdmission<'a, Backend: PageBackend, Account: MemoryAccount> {
    inner: &'a VmoInner<Backend, Account>,
    armed: bool,
}

impl<'a, Backend: PageBackend, Account: MemoryAccount>
    ExclusiveHardwareAdmission<'a, Backend, Account>
{
    fn acquire(inner: &'a VmoInner<Backend, Account>) -> VmoResult<Backend, Account, Self> {
        inner
            .access_state
            .compare_exchange(
                0,
                EXCLUSIVE_HARDWARE_BIT,
                Ordering::Acquire,
                Ordering::Relaxed,
            )
            .map_err(|_| VmoError::Busy)?;
        Ok(Self { inner, armed: true })
    }

    fn commit(mut self) {
        self.armed = false;
    }
}

impl<Backend: PageBackend, Account: MemoryAccount> Drop
    for ExclusiveHardwareAdmission<'_, Backend, Account>
{
    fn drop(&mut self) {
        if self.armed {
            release_exclusive_hardware_admission(self.inner);
        }
    }
}

impl<'a, Backend: PageBackend, Account: MemoryAccount> MappingAdmission<'a, Backend, Account> {
    fn acquire(inner: &'a VmoInner<Backend, Account>) -> VmoResult<Backend, Account, Self> {
        acquire_mapping_admission(inner)?;
        Ok(Self { inner, armed: true })
    }

    fn commit(mut self) {
        self.armed = false;
    }
}

impl<Backend: PageBackend, Account: MemoryAccount> Drop for MappingAdmission<'_, Backend, Account> {
    fn drop(&mut self) {
        if self.armed {
            release_mapping_admission(self.inner);
        }
    }
}

impl<'a, Backend: PageBackend, Account: MemoryAccount> KernelAccessGuard<'a, Backend, Account> {
    fn acquire(
        inner: &'a VmoInner<Backend, Account>,
    ) -> Result<Self, VmoError<Backend::Error, Account::Error>> {
        acquire_kernel_access(inner)?;
        Ok(Self { inner })
    }
}

impl<Backend: PageBackend, Account: MemoryAccount> Drop
    for KernelAccessGuard<'_, Backend, Account>
{
    fn drop(&mut self) {
        release_kernel_access(self.inner);
    }
}

fn acquire_kernel_access<Backend: PageBackend, Account: MemoryAccount>(
    inner: &VmoInner<Backend, Account>,
) -> VmoResult<Backend, Account, ()> {
    let mut current = inner.access_state.load(Ordering::Relaxed);
    loop {
        if current & (SNAPSHOT_BIT | EXCLUSIVE_HARDWARE_BIT | MAPPING_ACCESS_MASK) != 0 {
            return Err(VmoError::Busy);
        }
        let next = current.checked_add(1).ok_or(VmoError::SizeOverflow)?;
        if next & !KERNEL_ACCESS_MASK != 0 {
            return Err(VmoError::SizeOverflow);
        }
        match inner.access_state.compare_exchange_weak(
            current,
            next,
            Ordering::Acquire,
            Ordering::Relaxed,
        ) {
            Ok(_) => return Ok(()),
            Err(observed) => current = observed,
        }
    }
}

fn release_kernel_access<Backend: PageBackend, Account: MemoryAccount>(
    inner: &VmoInner<Backend, Account>,
) {
    let previous = inner.access_state.fetch_sub(1, Ordering::Release);
    if previous & KERNEL_ACCESS_MASK == 0 || previous & !KERNEL_ACCESS_MASK != 0 {
        vmo_invariant_violation();
    }
}

fn acquire_mapping_admission<Backend: PageBackend, Account: MemoryAccount>(
    inner: &VmoInner<Backend, Account>,
) -> VmoResult<Backend, Account, ()> {
    let mut current = inner.access_state.load(Ordering::Relaxed);
    loop {
        if current & (SNAPSHOT_BIT | EXCLUSIVE_HARDWARE_BIT | KERNEL_ACCESS_MASK) != 0 {
            return Err(VmoError::Busy);
        }
        let next = current
            .checked_add(MAPPING_UNIT)
            .ok_or(VmoError::SizeOverflow)?;
        if next & (SNAPSHOT_BIT | EXCLUSIVE_HARDWARE_BIT) != 0 {
            return Err(VmoError::SizeOverflow);
        }
        match inner.access_state.compare_exchange_weak(
            current,
            next,
            Ordering::Acquire,
            Ordering::Relaxed,
        ) {
            Ok(_) => return Ok(()),
            Err(observed) => current = observed,
        }
    }
}

fn release_mapping_admission<Backend: PageBackend, Account: MemoryAccount>(
    inner: &VmoInner<Backend, Account>,
) {
    let previous = inner
        .access_state
        .fetch_sub(MAPPING_UNIT, Ordering::Release);
    if previous & MAPPING_ACCESS_MASK == 0
        || previous & (SNAPSHOT_BIT | EXCLUSIVE_HARDWARE_BIT | KERNEL_ACCESS_MASK) != 0
    {
        vmo_invariant_violation();
    }
}

fn release_exclusive_hardware_admission<Backend: PageBackend, Account: MemoryAccount>(
    inner: &VmoInner<Backend, Account>,
) {
    if inner
        .access_state
        .compare_exchange(
            EXCLUSIVE_HARDWARE_BIT,
            0,
            Ordering::Release,
            Ordering::Relaxed,
        )
        .is_err()
    {
        vmo_invariant_violation();
    }
}

struct SnapshotGuard<'a, Backend: PageBackend, Account: MemoryAccount> {
    inner: &'a VmoInner<Backend, Account>,
}

impl<'a, Backend: PageBackend, Account: MemoryAccount> SnapshotGuard<'a, Backend, Account> {
    fn acquire(
        inner: &'a VmoInner<Backend, Account>,
    ) -> Result<Self, VmoError<Backend::Error, Account::Error>> {
        inner
            .access_state
            .compare_exchange(0, SNAPSHOT_BIT, Ordering::Acquire, Ordering::Relaxed)
            .map(|_| Self { inner })
            .map_err(|_| VmoError::Busy)
    }
}

impl<Backend: PageBackend, Account: MemoryAccount> Drop for SnapshotGuard<'_, Backend, Account> {
    fn drop(&mut self) {
        if self
            .inner
            .access_state
            .compare_exchange(SNAPSHOT_BIT, 0, Ordering::Release, Ordering::Relaxed)
            .is_err()
        {
            vmo_invariant_violation();
        }
    }
}

fn read_owned_inner<Backend: PageBackend, Account: MemoryAccount>(
    inner: &VmoInner<Backend, Account>,
    offset: u64,
    destination: &mut [u8],
) -> Result<(), VmoError<Backend::Error, Account::Error>> {
    validate_range(inner.size, offset, destination.len())?;
    visit_chunks(
        offset,
        destination.len(),
        |index, page_offset, target_offset, length| {
            let Some(page) = optional_page_ref(inner, index)? else {
                destination[target_offset..target_offset + length].fill(0);
                return Ok(());
            };
            page.with(|owned| {
                inner
                    .backend
                    .read_owned(
                        &owned.page,
                        page_offset,
                        &mut destination[target_offset..target_offset + length],
                    )
                    .map_err(VmoError::Backend)
            })
        },
    )
}

fn read_exposed_inner<Backend: PageBackend, Account: MemoryAccount>(
    inner: &VmoInner<Backend, Account>,
    offset: u64,
    destination: &mut [u8],
) -> VmoResult<Backend, Account, ()> {
    validate_range(inner.size, offset, destination.len())?;
    visit_chunks(
        offset,
        destination.len(),
        |index, page_offset, target_offset, length| {
            let page = page_ref(inner, index)?;
            page.with(|owned| {
                inner
                    .backend
                    .read_exposed(
                        &owned.page,
                        page_offset,
                        &mut destination[target_offset..target_offset + length],
                    )
                    .map_err(VmoError::Backend)
            })
        },
    )
}

fn write_exposed_inner<Backend: PageBackend, Account: MemoryAccount>(
    inner: &VmoInner<Backend, Account>,
    offset: u64,
    source: &[u8],
) -> VmoResult<Backend, Account, ()> {
    validate_range(inner.size, offset, source.len())?;
    visit_chunks(
        offset,
        source.len(),
        |index, page_offset, source_offset, length| {
            let page = page_ref(inner, index)?;
            page.with(|owned| {
                inner
                    .backend
                    .write_exposed(
                        &mut owned.page,
                        page_offset,
                        &source[source_offset..source_offset + length],
                    )
                    .map_err(VmoError::Backend)
            })
        },
    )
}

fn optional_page_ref<Backend: PageBackend, Account: MemoryAccount>(
    inner: &VmoInner<Backend, Account>,
    index: usize,
) -> VmoResult<Backend, Account, Option<PageRef<Backend, Account>>> {
    inner.state.with(|state| {
        state
            .pages
            .get(index)
            .ok_or(VmoError::InvalidRange)
            .cloned()
    })
}

fn resident_count<Backend: PageBackend, Account: MemoryAccount>(
    inner: &VmoInner<Backend, Account>,
    first: usize,
    count: usize,
) -> VmoResult<Backend, Account, usize> {
    let end = first.checked_add(count).ok_or(VmoError::SizeOverflow)?;
    let mut resident = 0;
    for index in first..end {
        if optional_page_ref(inner, index)?.is_some() {
            resident += 1;
        }
    }
    Ok(resident)
}

fn page_ref<Backend: PageBackend, Account: MemoryAccount>(
    inner: &VmoInner<Backend, Account>,
    index: usize,
) -> Result<PageRef<Backend, Account>, VmoError<Backend::Error, Account::Error>> {
    optional_page_ref(inner, index)?.ok_or(VmoError::InvalidRange)
}

#[cfg(test)]
pub(super) fn resident_pages<Backend: PageBackend, Account: MemoryAccount>(
    object: &MappingObject<Backend, Account>,
    offset: u64,
    length: u64,
) -> VmoResult<Backend, Account, Vec<Option<PhysicalAddress>>> {
    let length_usize = usize::try_from(length).map_err(|_| VmoError::SizeOverflow)?;
    validate_range(object.size(), offset, length_usize)?;
    let (first, count) = covered_pages(offset, length_usize)?;
    let mut result = Vec::new();
    result
        .try_reserve_exact(count)
        .map_err(|_| VmoError::Allocation)?;
    for index in first..first + count {
        result.push(match object.physical_page(index as u64 * PAGE_SIZE) {
            Ok(page) => Some(page),
            Err(VmoError::BackingNotResident) if object.private().is_some_and(|v| v.is_file()) => {
                None
            }
            Err(error) => return Err(error),
        });
    }
    Ok(result)
}

fn page_count<BackendError, AccountError>(
    size: u64,
) -> Result<usize, VmoError<BackendError, AccountError>> {
    if size == 0 {
        return Err(VmoError::InvalidRange);
    }
    if !size.is_multiple_of(PAGE_SIZE) {
        return Err(VmoError::InvalidRange);
    }
    let rounded = size
        .checked_add(PAGE_SIZE - 1)
        .ok_or(VmoError::SizeOverflow)?;
    usize::try_from(rounded / PAGE_SIZE).map_err(|_| VmoError::SizeOverflow)
}

fn covered_pages<BackendError, AccountError>(
    offset: u64,
    length: usize,
) -> Result<(usize, usize), VmoError<BackendError, AccountError>> {
    if length == 0 {
        return Ok((0, 0));
    }
    let first = usize::try_from(offset / PAGE_SIZE).map_err(|_| VmoError::SizeOverflow)?;
    let length = u64::try_from(length).map_err(|_| VmoError::SizeOverflow)?;
    let end = offset.checked_add(length).ok_or(VmoError::SizeOverflow)?;
    let last = usize::try_from((end - 1) / PAGE_SIZE).map_err(|_| VmoError::SizeOverflow)?;
    let count = last
        .checked_sub(first)
        .and_then(|distance| distance.checked_add(1))
        .ok_or(VmoError::SizeOverflow)?;
    Ok((first, count))
}

fn metadata_charge<Backend: PageBackend, Account: MemoryAccount>(
    pages: usize,
) -> Result<MemoryCharge, VmoError<Backend::Error, Account::Error>> {
    let slots = pages
        .checked_mul(size_of::<Option<PageRef<Backend, Account>>>())
        .ok_or(VmoError::SizeOverflow)?;
    let bytes = FallibleArc::<VmoInner<Backend, Account>>::allocation_size()
        .checked_add(slots)
        .ok_or(VmoError::SizeOverflow)?;
    Ok(MemoryCharge {
        kernel_bytes: u64::try_from(bytes).map_err(|_| VmoError::SizeOverflow)?,
        kernel_objects: 1,
        ..MemoryCharge::default()
    })
}

fn validate_range<BackendError, AccountError>(
    size: u64,
    offset: u64,
    length: usize,
) -> Result<(), VmoError<BackendError, AccountError>> {
    let length = u64::try_from(length).map_err(|_| VmoError::SizeOverflow)?;
    let end = offset.checked_add(length).ok_or(VmoError::SizeOverflow)?;
    if end > size {
        return Err(VmoError::InvalidRange);
    }
    Ok(())
}

fn visit_chunks<BackendError, AccountError>(
    mut offset: u64,
    length: usize,
    mut operation: impl FnMut(
        usize,
        usize,
        usize,
        usize,
    ) -> Result<(), VmoError<BackendError, AccountError>>,
) -> Result<(), VmoError<BackendError, AccountError>> {
    let mut visited = 0;
    while visited < length {
        let index = usize::try_from(offset / PAGE_SIZE).map_err(|_| VmoError::SizeOverflow)?;
        let page_offset =
            usize::try_from(offset % PAGE_SIZE).map_err(|_| VmoError::SizeOverflow)?;
        let chunk = (PAGE_SIZE as usize - page_offset).min(length - visited);
        operation(index, page_offset, visited, chunk)?;
        visited = visited.checked_add(chunk).ok_or(VmoError::SizeOverflow)?;
        offset = offset
            .checked_add(chunk as u64)
            .ok_or(VmoError::SizeOverflow)?;
    }
    Ok(())
}

fn map_allocation<BackendError, AccountError>(
    _: AllocationError,
) -> VmoError<BackendError, AccountError> {
    VmoError::Allocation
}

const SNAPSHOT_BIT: usize = 1usize << (usize::BITS - 1);
const EXCLUSIVE_HARDWARE_BIT: usize = 1usize << (usize::BITS - 2);
const MAPPING_UNIT: usize = 1usize << (usize::BITS / 2);
const KERNEL_ACCESS_MASK: usize = MAPPING_UNIT - 1;
const MAPPING_ACCESS_MASK: usize = EXCLUSIVE_HARDWARE_BIT - MAPPING_UNIT;

#[cold]
fn vmo_invariant_violation() -> ! {
    hyper::debug::invariant_failure("VMO access ownership invariant")
}
