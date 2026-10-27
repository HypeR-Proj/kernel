// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Capability-scoped task and object observability.
//!
//! Inspector objects carry immutable scope and visibility policy. Diagnostic
//! identifiers and cursors are deliberately observation-only: neither can be
//! converted back into operational authority.

use crate::authority::Rights;
use {
    crate::accounting::CommittedCharge, crate::accounting::ResourceAmount,
    crate::accounting::ResourceDomain, crate::accounting::ResourceError,
    crate::accounting::ResourceKind,
};
use {crate::capability::HandleScanCursor, crate::capability::HandleSnapshot};
use {
    crate::object::KernelObject, crate::object::ObjectCreationError, crate::object::ObjectKind,
    crate::object::ObjectSnapshot, crate::object::TransferClass,
    crate::object::object_allocation_size, crate::object::private,
};
use {
    crate::process::ProcessId, crate::process::ProcessObject, crate::process::ProcessScanCursor,
    crate::process::ProcessSnapshot, crate::process::TaskGroupId,
};
use {
    crate::task::ThreadObjectRegistryPhase, crate::task::ThreadObjectScanCursor,
    crate::task::ThreadRole,
};

pub(crate) mod memory;

// Keep capture pages deliberately small: scans execute on 16-KiB kernel
// stacks, and source directory snapshots coexist with ABI encoding frames.
pub(crate) const PROCESS_PAGE_CAPACITY: usize = 8;
pub(crate) const THREAD_PAGE_CAPACITY: usize = 8;
pub(crate) const OBJECT_PAGE_CAPACITY: usize = 8;
pub(crate) const HANDLE_PAGE_CAPACITY: usize = 8;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Scope {
    System,
    ResourceDomain(crate::accounting::ResourceDomainId),
    TaskGroup(TaskGroupId),
    Process(ProcessId),
}

impl Scope {
    fn permits(self, process: ProcessSnapshot) -> bool {
        match self {
            Self::System => true,
            Self::ResourceDomain(domain) => process.domain_id == domain,
            Self::TaskGroup(group) => process.group_id == group,
            Self::Process(id) => process.id == id,
        }
    }

    fn permits_task_group(
        self,
        group: TaskGroupId,
        domain: crate::accounting::ResourceDomainId,
    ) -> bool {
        match self {
            Self::System => true,
            Self::ResourceDomain(permitted) => permitted == domain,
            Self::TaskGroup(permitted) => permitted == group,
            Self::Process(_) => false,
        }
    }

    fn permits_resource_domain(self, domain: crate::accounting::ResourceDomainId) -> bool {
        match self {
            Self::System => true,
            Self::ResourceDomain(permitted) => permitted == domain,
            Self::TaskGroup(_) | Self::Process(_) => false,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct TaskVisibility(u32);

impl TaskVisibility {
    const PROCESS_BASIC: Self = Self(1 << 0);
    const THREAD_BASIC: Self = Self(1 << 1);
    const KERNEL_THREADS: Self = Self(1 << 2);
    const SYSTEM: Self =
        Self(Self::PROCESS_BASIC.0 | Self::THREAD_BASIC.0 | Self::KERNEL_THREADS.0);

    const fn contains(self, required: Self) -> bool {
        self.0 & required.0 == required.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ObjectVisibility(u32);

impl ObjectVisibility {
    const HANDLE_BASIC: Self = Self(1 << 0);
    const OBJECT_BASIC: Self = Self(1 << 1);
    const REFERENCE_COUNTS: Self = Self(1 << 2);
    const KERNEL_OBJECTS: Self = Self(1 << 3);
    const SYSTEM: Self = Self(
        Self::HANDLE_BASIC.0
            | Self::OBJECT_BASIC.0
            | Self::REFERENCE_COUNTS.0
            | Self::KERNEL_OBJECTS.0,
    );

    const fn contains(self, required: Self) -> bool {
        self.0 & required.0 == required.0
    }
}

#[derive(Debug)]
pub(crate) enum Error {
    Unsupported,
    AccessDenied,
    Details(crate::object::diagnostics::DetailError),
    Allocation,
    NotFound,
    Object(ObjectCreationError),
    Process(crate::process::ProcessError),
    Resource(ResourceError),
    Scheduler(crate::task::scheduler::Error),
    Unavailable,
    InconsistentAccounting,
}

impl From<ObjectCreationError> for Error {
    fn from(error: ObjectCreationError) -> Self {
        Self::Object(error)
    }
}

impl From<ResourceError> for Error {
    fn from(error: ResourceError) -> Self {
        Self::Resource(error)
    }
}

impl From<crate::process::ProcessError> for Error {
    fn from(error: crate::process::ProcessError) -> Self {
        Self::Process(error)
    }
}

impl From<crate::task::scheduler::Error> for Error {
    fn from(error: crate::task::scheduler::Error) -> Self {
        Self::Scheduler(error)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TaskThreadSnapshot {
    pub(crate) koid: crate::object::Koid,
    pub(crate) process_koid: Option<crate::object::Koid>,
    pub(crate) name: crate::task::scheduler::ThreadNameSnapshot,
    pub(crate) role: ThreadRole,
    pub(crate) registry_phase: ThreadObjectRegistryPhase,
    pub(crate) runtime_ns: u64,
}

pub(crate) struct Page<T: Copy, const N: usize> {
    entries: [Option<T>; N],
    len: usize,
    next: u64,
}

impl<T: Copy, const N: usize> Page<T, N> {
    pub(crate) const fn empty() -> Self {
        Self {
            entries: [None; N],
            len: 0,
            next: 0,
        }
    }

    fn push(&mut self, entry: T) {
        if self.len >= N {
            hyper::debug::invariant_failure("inspect::mod::push invariant");
        }
        self.entries[self.len] = Some(entry);
        self.len += 1;
    }

    pub(crate) fn entries(&self) -> impl Iterator<Item = &T> {
        self.entries[..self.len].iter().filter_map(Option::as_ref)
    }

    pub(crate) const fn len(&self) -> usize {
        self.len
    }

    pub(crate) const fn next(&self) -> u64 {
        self.next
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ProcessHandleSnapshot {
    pub(crate) process_koid: crate::object::Koid,
    pub(crate) handle: HandleSnapshot,
}

/// Immutable authority to observe a bounded task hierarchy.
pub(crate) struct TaskInspector {
    scope: Scope,
    visibility: TaskVisibility,
    _object_charge: CommittedCharge,
}

impl TaskInspector {
    pub(crate) fn try_system(domain: &ResourceDomain) -> Result<Self, Error> {
        Ok(Self {
            scope: Scope::System,
            visibility: TaskVisibility::SYSTEM,
            _object_charge: reserve_object_charge::<Self>(domain)?,
        })
    }

    pub(crate) fn try_derive_process(
        &self,
        process: &ProcessObject,
        sponsor: &ResourceDomain,
    ) -> Result<Self, Error> {
        let target = process.snapshot();
        if !self.scope.permits(target) {
            return Err(Error::NotFound);
        }
        Ok(Self {
            scope: Scope::Process(target.id),
            visibility: TaskVisibility(self.visibility.0 & !TaskVisibility::KERNEL_THREADS.0),
            _object_charge: reserve_object_charge::<Self>(sponsor)?,
        })
    }

    pub(crate) fn try_derive_task_group(
        &self,
        group: &crate::process::TaskGroupObject,
        sponsor: &ResourceDomain,
    ) -> Result<Self, Error> {
        let group = group.group();
        if !self.scope.permits_task_group(group.id(), group.domain_id()) {
            return Err(Error::NotFound);
        }
        Ok(Self {
            scope: Scope::TaskGroup(group.id()),
            visibility: TaskVisibility(self.visibility.0 & !TaskVisibility::KERNEL_THREADS.0),
            _object_charge: reserve_object_charge::<Self>(sponsor)?,
        })
    }

    pub(crate) fn try_derive_resource_domain(
        &self,
        domain: &crate::accounting::ResourceDomainObject,
        sponsor: &ResourceDomain,
    ) -> Result<Self, Error> {
        if !self.scope.permits_resource_domain(domain.domain().id()) {
            return Err(Error::NotFound);
        }
        Ok(Self {
            scope: Scope::ResourceDomain(domain.domain().id()),
            visibility: TaskVisibility(self.visibility.0 & !TaskVisibility::KERNEL_THREADS.0),
            _object_charge: reserve_object_charge::<Self>(sponsor)?,
        })
    }

    pub(crate) fn scan_processes(
        &self,
        cursor: u64,
        output: &mut Page<ProcessSnapshot, PROCESS_PAGE_CAPACITY>,
    ) -> Result<(), Error> {
        if !self.visibility.contains(TaskVisibility::PROCESS_BASIC) {
            return Err(Error::AccessDenied);
        }
        let source = crate::process::scan(ProcessScanCursor::from_token(cursor));
        output.len = 0;
        output.next = 0;
        for process in source.entries() {
            let snapshot = process.snapshot();
            if self.scope.permits(snapshot) {
                output.push(snapshot);
            }
        }
        output.next = source.next().map_or(0, ProcessScanCursor::token);
        Ok(())
    }

    /// Fills caller-owned storage without moving a full snapshot page through
    /// the scheduler and ABI adapters. Only a successful result is publishable.
    pub(crate) fn scan_threads(
        &self,
        cursor: u64,
        output: &mut Page<TaskThreadSnapshot, THREAD_PAGE_CAPACITY>,
    ) -> Result<(), Error> {
        if !self.visibility.contains(TaskVisibility::THREAD_BASIC) {
            return Err(Error::AccessDenied);
        }
        let token = usize::try_from(cursor).map_err(|_| Error::NotFound)?;
        let mut source = crate::task::ThreadObjectSnapshotPage::empty();
        crate::task::scheduler::scan_thread_objects(
            ThreadObjectScanCursor::from_token(token),
            &mut source,
        )?;
        output.len = 0;
        output.next = 0;
        for thread in source.entries() {
            let (process, permitted) = match thread.object.process {
                Some(id) => {
                    let process = find_process(id);
                    let permitted = process.is_some_and(|snapshot| self.scope.permits(snapshot));
                    (process, permitted)
                }
                None => (
                    None,
                    self.scope == Scope::System
                        && self.visibility.contains(TaskVisibility::KERNEL_THREADS),
                ),
            };
            if permitted {
                output.push(TaskThreadSnapshot {
                    koid: thread.object.object.koid,
                    process_koid: process.map(|process| process.koid),
                    name: thread.name,
                    role: thread.object.role,
                    registry_phase: thread.phase,
                    runtime_ns: thread.runtime_ns,
                });
            }
        }
        output.next = match source.next() {
            Some(next) => u64::try_from(next.token()).map_err(|_| Error::Allocation)?,
            None => 0,
        };
        Ok(())
    }
}

impl private::Sealed for TaskInspector {}
impl private::UserExportable for TaskInspector {}

impl KernelObject for TaskInspector {
    const KIND: ObjectKind = ObjectKind::TASK_INSPECTOR;
    const TRANSFER_CLASS: TransferClass = TransferClass::Leaf;
    const SUPPORTED_RIGHTS: Rights = Rights::DUPLICATE
        .union(Rights::TRANSFER)
        .union(Rights::INSPECT)
        .union(Rights::DERIVE);
}

/// Immutable authority to observe kernel objects and Process handle edges.
pub(crate) struct ObjectInspector {
    scope: Scope,
    visibility: ObjectVisibility,
    _object_charge: CommittedCharge,
}

impl ObjectInspector {
    /// Derivation deliberately returns the basic observation authority only.
    pub(crate) const DERIVATION_RIGHTS: Rights = Rights::DUPLICATE
        .union(Rights::TRANSFER)
        .union(Rights::INSPECT)
        .union(Rights::DERIVE);

    pub(crate) fn read_details(
        &self,
        process: u64,
        target: u64,
        cursor: u64,
    ) -> Result<crate::object::diagnostics::ObjectDetails, Error> {
        // Scope checks precede target lookup so denied queries cannot probe liveness.
        let object = if process == 0 {
            if self.scope != Scope::System
                || !self.visibility.contains(ObjectVisibility::KERNEL_OBJECTS)
            {
                return Err(Error::AccessDenied);
            }
            crate::object::lookup_diagnostic(target).ok_or(Error::NotFound)?
        } else {
            if !self.visibility.contains(ObjectVisibility::HANDLE_BASIC)
                || !self.visibility.contains(ObjectVisibility::OBJECT_BASIC)
            {
                return Err(Error::AccessDenied);
            }
            let process = find_process_by_koid(process, self.scope).ok_or(Error::NotFound)?;
            let handle =
                crate::capability::handle_wire::from_abi(target).map_err(|_| Error::NotFound)?;
            process
                .inspect_handle_object(handle)
                .map_err(|error| match error {
                    crate::process::ProcessError::Handle(_) => Error::NotFound,
                    error => Error::Process(error),
                })?
        };
        // No registry, process state, or handle-table lock survives this point.
        object.details(cursor).map_err(Error::Details)
    }

    pub(crate) fn try_system(domain: &ResourceDomain) -> Result<Self, Error> {
        Ok(Self {
            scope: Scope::System,
            visibility: ObjectVisibility::SYSTEM,
            _object_charge: reserve_object_charge::<Self>(domain)?,
        })
    }

    pub(crate) fn try_derive_process(
        &self,
        process: &ProcessObject,
        sponsor: &ResourceDomain,
    ) -> Result<Self, Error> {
        let target = process.snapshot();
        if !self.scope.permits(target) {
            return Err(Error::NotFound);
        }
        Ok(Self {
            scope: Scope::Process(target.id),
            visibility: ObjectVisibility(
                self.visibility.0
                    & (ObjectVisibility::HANDLE_BASIC.0 | ObjectVisibility::OBJECT_BASIC.0),
            ),
            _object_charge: reserve_object_charge::<Self>(sponsor)?,
        })
    }

    pub(crate) fn try_derive_task_group(
        &self,
        group: &crate::process::TaskGroupObject,
        sponsor: &ResourceDomain,
    ) -> Result<Self, Error> {
        let group = group.group();
        if !self.scope.permits_task_group(group.id(), group.domain_id()) {
            return Err(Error::NotFound);
        }
        Ok(Self {
            scope: Scope::TaskGroup(group.id()),
            visibility: ObjectVisibility(
                self.visibility.0
                    & (ObjectVisibility::HANDLE_BASIC.0 | ObjectVisibility::OBJECT_BASIC.0),
            ),
            _object_charge: reserve_object_charge::<Self>(sponsor)?,
        })
    }

    pub(crate) fn try_derive_resource_domain(
        &self,
        domain: &crate::accounting::ResourceDomainObject,
        sponsor: &ResourceDomain,
    ) -> Result<Self, Error> {
        if !self.scope.permits_resource_domain(domain.domain().id()) {
            return Err(Error::NotFound);
        }
        Ok(Self {
            scope: Scope::ResourceDomain(domain.domain().id()),
            visibility: ObjectVisibility(
                self.visibility.0
                    & (ObjectVisibility::HANDLE_BASIC.0 | ObjectVisibility::OBJECT_BASIC.0),
            ),
            _object_charge: reserve_object_charge::<Self>(sponsor)?,
        })
    }

    pub(crate) fn scan_objects(
        &self,
        cursor: u64,
        output: &mut Page<ObjectSnapshot, OBJECT_PAGE_CAPACITY>,
    ) -> Result<(), Error> {
        if self.scope != Scope::System
            || !self.visibility.contains(ObjectVisibility::KERNEL_OBJECTS)
        {
            return Err(Error::AccessDenied);
        }
        let source = crate::object::scan(crate::object::ObjectScanCursor::from_token(cursor));
        output.len = 0;
        output.next = 0;
        for object in source.entries() {
            output.push(*object);
        }
        output.next = source
            .next()
            .map_or(0, crate::object::ObjectScanCursor::token);
        Ok(())
    }

    pub(crate) fn scan_process_handles(
        &self,
        process_koid: u64,
        cursor: u64,
    ) -> Result<Page<ProcessHandleSnapshot, HANDLE_PAGE_CAPACITY>, Error> {
        if !self.visibility.contains(ObjectVisibility::HANDLE_BASIC)
            || !self.visibility.contains(ObjectVisibility::OBJECT_BASIC)
        {
            return Err(Error::AccessDenied);
        }
        let process = find_process_by_koid(process_koid, self.scope).ok_or(Error::NotFound)?;
        let token = usize::try_from(cursor).map_err(|_| Error::NotFound)?;
        let source = process
            .scan_handles(HandleScanCursor::from_token(token))
            .map_err(|error| match error {
                // A diagnostic pin keeps the Process alive, not its handle
                // table. Exit may retire the table after directory lookup.
                crate::process::ProcessError::Handle(
                    crate::capability::HandleError::TableRetired,
                ) => Error::NotFound,
                error => Error::Process(error),
            })?;
        let snapshot = process.snapshot();
        let mut output = Page::empty();
        for handle in source.entries() {
            output.push(ProcessHandleSnapshot {
                process_koid: snapshot.koid,
                handle: *handle,
            });
        }
        output.next = match source.next() {
            Some(next) => u64::try_from(next.token()).map_err(|_| Error::Allocation)?,
            None => 0,
        };
        Ok(output)
    }
}

impl private::Sealed for ObjectInspector {}
impl private::UserExportable for ObjectInspector {}

impl KernelObject for ObjectInspector {
    const KIND: ObjectKind = ObjectKind::OBJECT_INSPECTOR;
    const TRANSFER_CLASS: TransferClass = TransferClass::Leaf;
    const SUPPORTED_RIGHTS: Rights = Self::DERIVATION_RIGHTS.union(Rights::INSPECT_DETAILS);
}

/// Immutable physical-memory accounting snapshot.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct MemoryObservation {
    pub(crate) captured_at_ns: u64,
    pub(crate) page_size: u64,
    pub(crate) total_bytes: u64,
    pub(crate) managed_bytes: u64,
    pub(crate) free_bytes: u64,
    pub(crate) kernel_bytes: u64,
    pub(crate) slab_bytes: u64,
    pub(crate) large_heap_bytes: u64,
    pub(crate) page_table_bytes: u64,
    pub(crate) vmo_bytes: u64,
    pub(crate) guest_loaned_bytes: u64,
    pub(crate) guest_private_bytes: u64,
    pub(crate) file_cache_bytes: u64,
    pub(crate) slab_reclaimable_bytes: u64,
    pub(crate) slab_sample_complete: u64,
    pub(crate) file_cache_reclaimable_bytes: u64,
    pub(crate) file_cache_sample_complete: u64,
    pub(crate) ramfs_capacity_bytes: u64,
}

/// Read-only authority over system physical-memory accounting.
pub(crate) struct MemoryInspector {
    _object_charge: CommittedCharge,
}

impl MemoryInspector {
    #[cfg_attr(
        all(feature = "kernel-test", feature = "kernel-self-test"),
        expect(
            dead_code,
            reason = "The kernel self-test image does not enter Native process bootstrap"
        )
    )]
    pub(crate) fn try_system(domain: &ResourceDomain) -> Result<Self, Error> {
        Ok(Self {
            _object_charge: reserve_object_charge::<Self>(domain)?,
        })
    }

    pub(crate) fn snapshot(&self) -> Result<MemoryObservation, Error> {
        let stats = crate::mm::statistics().ok_or(Error::Unavailable)?;
        let page_size = hyper::mm::PAGE_SIZE;
        let pages_to_bytes = |pages: usize| {
            u64::try_from(pages)
                .ok()
                .and_then(|pages| pages.checked_mul(page_size))
                .ok_or(Error::InconsistentAccounting)
        };
        let runtime = stats.runtime;
        let buddy = runtime.buddy;
        // Physical categories come from one allocator capture. Reclaimability
        // is sampled independently and carries separate validity for each pool;
        // neither a partial sample nor userspace presentation policy is summed
        // into the physical counters here.
        let file_reclaimable =
            crate::fs::vfs::file_cache().and_then(|cache| cache.reclaimable_pages());
        // The shrinker owns both payloads and their index/header backing.
        // Metadata grows with cached files and contracts when they are evicted;
        // excluding it makes a warmed cache look like permanently used RAM.
        // This is a shrinkable-pool estimate, not a promise that every page can
        // be freed immediately: live pins can retain a minimum index/slab set.
        let file_reclaimable = file_reclaimable.map(|payloads| {
            payloads.min(runtime.file_cache_pages.pages)
                + runtime.file_cache_slab_pages
                + runtime.file_cache_large_pages
        });
        let slab_reclaimable = runtime.cache.reclaimable_pages;
        Ok(MemoryObservation {
            captured_at_ns: crate::time::monotonic_nanoseconds().unwrap_or_default(),
            page_size,
            total_bytes: pages_to_bytes(stats.boot.ram_pages)?,
            managed_bytes: pages_to_bytes(buddy.managed_pages)?,
            free_bytes: pages_to_bytes(buddy.free_pages)?,
            kernel_bytes: pages_to_bytes(runtime.kernel_pages.pages)?,
            slab_bytes: pages_to_bytes(runtime.slab_pages - runtime.file_cache_slab_pages)?,
            large_heap_bytes: pages_to_bytes(
                runtime.large_heap_pages - runtime.file_cache_large_pages,
            )?,
            page_table_bytes: pages_to_bytes(runtime.page_table_pages.pages)?,
            vmo_bytes: pages_to_bytes(runtime.user_pages.pages)?,
            guest_loaned_bytes: pages_to_bytes(runtime.guest_loaned_pages)?,
            guest_private_bytes: pages_to_bytes(runtime.guest_pages.pages)?,
            file_cache_bytes: pages_to_bytes(
                runtime.file_cache_pages.pages
                    + runtime.file_cache_slab_pages
                    + runtime.file_cache_large_pages,
            )?,
            slab_reclaimable_bytes: pages_to_bytes(slab_reclaimable.unwrap_or(0))?,
            slab_sample_complete: u64::from(slab_reclaimable.is_some()),
            file_cache_reclaimable_bytes: pages_to_bytes(file_reclaimable.unwrap_or(0))?,
            file_cache_sample_complete: u64::from(file_reclaimable.is_some()),
            ramfs_capacity_bytes: crate::fs::vfs::ramfs_capacity_bytes(),
        })
    }
}

impl private::Sealed for MemoryInspector {}
impl private::UserExportable for MemoryInspector {}

impl KernelObject for MemoryInspector {
    const KIND: ObjectKind = ObjectKind::MEMORY_INSPECTOR;
    const TRANSFER_CLASS: TransferClass = TransferClass::Leaf;
    const SUPPORTED_RIGHTS: Rights = Rights::DUPLICATE
        .union(Rights::TRANSFER)
        .union(Rights::INSPECT);
}

/// Read-only authority over scheduler-owned CPU-time accounting.
pub(crate) struct CpuInspector {
    _object_charge: CommittedCharge,
}

impl CpuInspector {
    #[cfg_attr(
        all(feature = "kernel-test", feature = "kernel-self-test"),
        expect(
            dead_code,
            reason = "The kernel self-test image does not enter Native process bootstrap"
        )
    )]
    pub(crate) fn try_system(domain: &ResourceDomain) -> Result<Self, Error> {
        Ok(Self {
            _object_charge: reserve_object_charge::<Self>(domain)?,
        })
    }

    pub(crate) fn snapshot(&self) -> crate::task::scheduler::CpuTimeSnapshot {
        crate::task::scheduler::cpu_time_snapshot()
    }
}

impl private::Sealed for CpuInspector {}
impl private::UserExportable for CpuInspector {}

impl KernelObject for CpuInspector {
    const KIND: ObjectKind = ObjectKind::CPU_INSPECTOR;
    const TRANSFER_CLASS: TransferClass = TransferClass::Leaf;
    const SUPPORTED_RIGHTS: Rights = Rights::DUPLICATE
        .union(Rights::TRANSFER)
        .union(Rights::INSPECT)
        .union(Rights::SET_ATTRIBUTES)
        .union(Rights::INSPECT_DETAILS);
}

fn find_process(id: ProcessId) -> Option<ProcessSnapshot> {
    find_process_matching(|snapshot| snapshot.id == id).map(|process| process.snapshot())
}

fn find_process_by_koid(koid: u64, scope: Scope) -> Option<crate::process::ProcessDiagnosticRef> {
    find_process_matching(|snapshot| snapshot.koid.get() == koid && scope.permits(snapshot))
}

fn find_process_matching(
    mut predicate: impl FnMut(ProcessSnapshot) -> bool,
) -> Option<crate::process::ProcessDiagnosticRef> {
    let mut cursor = ProcessScanCursor::start();
    loop {
        let page = crate::process::scan(cursor);
        for process in page.entries() {
            if predicate(process.snapshot()) {
                return Some(process.clone_diagnostic());
            }
        }
        cursor = page.next()?;
    }
}

fn reserve_object_charge<T: KernelObject>(
    domain: &ResourceDomain,
) -> Result<CommittedCharge, Error> {
    let bytes = object_allocation_size::<T>()
        .and_then(|value| u64::try_from(value).ok())
        .ok_or(Error::Allocation)?;
    Ok(domain
        .reserve(
            ResourceAmount::ZERO
                .with(ResourceKind::KernelObjects, 1)
                .with(ResourceKind::KernelMemoryBytes, bytes),
        )?
        .commit())
}
