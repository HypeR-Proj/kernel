// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Stable scheduler identities and demand-allocated, address-stable slot storage.
//!
//! Published slot segments never move. Growth is prepared outside scheduler
//! locks and release-published without relocating existing cells. The transition
//! coordinator owns slot variants and control links. Locked per-CPU domains
//! access only the schedule cells of
//! Threads whose linear residence names that CPU.

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::cell::UnsafeCell;
use core::marker::PhantomData;
use core::sync::atomic::{AtomicPtr, Ordering};
use hyper::cpu::CpuIndex;

use super::Error;
use {
    crate::task::thread::Thread, crate::task::thread::ThreadId,
    crate::task::thread::ThreadScheduleState,
};

#[expect(
    clippy::large_enum_variant,
    reason = "retiring slots must preserve allocation-free thread diagnostics after ownership is detached"
)]
enum ThreadSlot {
    Vacant,
    Free(Option<usize>),
    Reserved(ThreadId),
    Occupied(Box<Thread>),
    /// Identity remains unavailable until lock-external resource teardown ends.
    Retiring {
        id: ThreadId,
        name: super::super::thread::ThreadNameSnapshot,
        // Snapshotting preserves generation-correct diagnostics without
        // extending object lifetime. The detached Thread carries the actual
        // scheduler reference into the reaper until its resources are gone.
        object: crate::task::ThreadObjectSnapshot,
        runtime_ns: u64,
        thread: Option<Box<Thread>>,
        retirement: Option<super::ResourceRetirement>,
        next: Option<ThreadId>,
    },
}

/// One address-stable cell in the scheduler's segmented thread table.
///
/// `UnsafeCell` is deliberately confined to this module. Slot mutation still
/// requires the transition coordinator's private authority, while per-CPU
/// capabilities expose only closure-bounded access to an occupied allocation.
/// No slot reference escapes either authority.
struct ThreadSlotCell(UnsafeCell<ThreadSlot>);

impl ThreadSlotCell {
    const fn new(slot: ThreadSlot) -> Self {
        Self(UnsafeCell::new(slot))
    }
}

// Exponential segments give constant-time lookup without moving a directory
// that another CPU may be reading. Only populated segments consume slot memory.
const FIRST_SEGMENT: usize = 64;
const SEGMENTS: usize = 19;
// The ready AVL traversal bound and segment directory cover this ID namespace.
const _: () = assert!(ThreadId::SLOT_BITS <= 24);

fn segment_for(slot: usize) -> usize {
    (usize::BITS - 1 - (slot / FIRST_SEGMENT + 1).leading_zeros()) as usize
}

fn segment_start(segment: usize) -> usize {
    FIRST_SEGMENT * ((1usize << segment) - 1)
}

pub(super) struct TableGrowth {
    segment: usize,
    cells: Option<Box<[ThreadSlotCell]>>,
}

impl TableGrowth {
    /// Allocation and initialization run before acquiring the coordinator.
    pub(super) fn prepare(segment: usize) -> Result<Self, Error> {
        if segment >= SEGMENTS {
            return Err(Error::IdentifierExhausted);
        }
        let count = FIRST_SEGMENT << segment;
        let mut cells = Vec::new();
        cells
            .try_reserve_exact(count)
            .map_err(|_| Error::Allocation)?;
        cells.resize_with(count, || ThreadSlotCell::new(ThreadSlot::Vacant));
        Ok(Self {
            segment,
            cells: Some(cells.into_boxed_slice()),
        })
    }
}

/// Directory and published segments remain alive for every CPU capability.
struct ThreadTable {
    segments: [AtomicPtr<ThreadSlotCell>; SEGMENTS],
}

impl Drop for ThreadTable {
    fn drop(&mut self) {
        // Only unpublished construction can drop a table. Published tables are
        // permanent; segment cells are recycled after resource retirement.
        for (index, pointer) in self.segments.iter_mut().enumerate() {
            let pointer = *pointer.get_mut();
            if !pointer.is_null() {
                let slice = core::ptr::slice_from_raw_parts_mut(pointer, FIRST_SEGMENT << index);
                // SAFETY: this table uniquely owns each installed boxed slice.
                drop(unsafe { Box::from_raw(slice) });
            }
        }
    }
}

// SAFETY: `ThreadTable` is permanently allocated and its UnsafeCell contents
// are reachable only through private authority values. Slot/control authority
// remains under TransitionLock; each schedule cell is mutated only through
// its matching locked CPU token. These domains access disjoint UnsafeCells and
// no authority exposes a slot reference.
unsafe impl Sync for ThreadTable {}

impl ThreadTable {
    fn slot(&self, index: usize) -> Option<&ThreadSlot> {
        let cell = self.cell(index)?;
        // SAFETY: coordinator readers pin slot lifetime through a reader
        // lane; writers own every lane. CPU-only authorities reference only
        // owned current/ready/blocked IDs, whose retirement needs that CPU lock.
        Some(unsafe { &*cell.0.get() })
    }

    #[allow(clippy::mut_from_ref)]
    fn slot_mut<'authority>(
        &'authority self,
        index: usize,
        _authority: &'authority mut ThreadTableAuthorityToken,
    ) -> Option<&'authority mut ThreadSlot> {
        let cell = self.cell(index)?;
        // SAFETY: callers must hold the unique private write-authority token.
        // The returned borrow is closure-bounded by that authority and cannot
        // escape into scheduler state.
        Some(unsafe { &mut *cell.0.get() })
    }

    fn cell(&self, index: usize) -> Option<&ThreadSlotCell> {
        let segment = segment_for(index);
        let pointer = self.segments.get(segment)?.load(Ordering::Acquire);
        if pointer.is_null() {
            return None;
        }
        // SAFETY: publication transfers an initialized segment of this exact
        // length. Its backing is permanent once CPU capabilities are published.
        Some(unsafe { &*pointer.add(index - segment_start(segment)) })
    }

    fn install(&self, growth: &mut TableGrowth, _authority: &mut ThreadTableAuthorityToken) {
        let destination = &self.segments[growth.segment];
        if !destination.load(Ordering::Relaxed).is_null() {
            return; // A racing creator already installed it; caller frees ours outside locks.
        }
        let cells = growth.cells.take().unwrap_or_else(|| registry_invariant());
        destination.store(
            Box::into_raw(cells).cast::<ThreadSlotCell>(),
            Ordering::Release,
        );
    }
}

/// Non-revocable reference to the permanently allocated thread table.
///
/// This is intentionally private and contains no raw address. It can later be
/// copied into per-CPU scheduler owners, while actual access still requires a
/// linear authority token.
#[derive(Clone, Copy)]
pub(super) struct ThreadTableCapability {
    table: &'static ThreadTable,
}

/// Linear proof that the matching per-CPU scheduler lock is held.
///
/// The token is stored inside `CpuScheduler`; only a mutable borrow obtained
/// while its lock is held can mint a CPU table authority.
pub(super) struct CpuScheduleAuthorityToken {
    _private: (),
}

impl CpuScheduleAuthorityToken {
    pub(super) const fn new() -> Self {
        Self { _private: () }
    }
}

/// Closure-bounded access to schedules owned by one locked CPU domain.
pub(super) struct CpuThreadTableAuthority<'cpu> {
    table: ThreadTableCapability,
    cpu: CpuIndex,
    _token: &'cpu mut CpuScheduleAuthorityToken,
}

impl ThreadTableCapability {
    pub(super) fn cpu_authority<'cpu>(
        self,
        cpu: CpuIndex,
        token: &'cpu mut CpuScheduleAuthorityToken,
    ) -> CpuThreadTableAuthority<'cpu> {
        CpuThreadTableAuthority {
            table: self,
            cpu,
            _token: token,
        }
    }

    pub(super) fn with_thread<R>(
        self,
        id: ThreadId,
        operation: impl for<'thread> FnOnce(&'thread Thread) -> R,
    ) -> Result<R, Error> {
        let slot = id.scheduler_slot().ok_or(Error::ThreadNotFound)?;
        match self.table.slot(slot) {
            Some(ThreadSlot::Occupied(thread)) if thread.id() == id => Ok(operation(thread)),
            _ => Err(Error::ThreadNotFound),
        }
    }
}

impl CpuThreadTableAuthority<'_> {
    pub(super) const fn cpu(&self) -> CpuIndex {
        self.cpu
    }

    pub(super) fn with_thread<R>(
        &self,
        id: ThreadId,
        operation: impl for<'thread> FnOnce(&'thread Thread, &'thread ThreadScheduleState) -> R,
    ) -> Result<R, Error> {
        self.table.with_thread(id, |thread| {
            // SAFETY: construction requires the mutable authority token held
            // inside the matching locked CpuScheduler. Owner is revalidated
            // by Thread before dereferencing its stable schedule cell.
            unsafe { thread.with_cpu_schedule(self.cpu, |schedule| operation(thread, schedule)) }
                .unwrap_or_else(|| registry_invariant())
        })
    }

    pub(super) fn with_thread_mut<R>(
        &mut self,
        id: ThreadId,
        operation: impl for<'thread> FnOnce(&'thread Thread, &'thread mut ThreadScheduleState) -> R,
    ) -> Result<R, Error> {
        self.table.with_thread(id, |thread| {
            // SAFETY: the unique mutable CPU authority token proves exclusive
            // access to this CPU domain for the closure duration.
            unsafe {
                thread.with_cpu_schedule_mut(self.cpu, |schedule| operation(thread, schedule))
            }
            .unwrap_or_else(|| registry_invariant())
        })
    }
}

#[derive(Clone, Copy)]
enum ThreadTableAccess<'table> {
    Staged(&'table ThreadTable),
    Published(ThreadTableCapability),
}

impl<'table> ThreadTableAccess<'table> {
    const fn table(self) -> &'table ThreadTable {
        match self {
            Self::Staged(table) => table,
            Self::Published(capability) => capability.table,
        }
    }
}

enum ThreadTableStorage {
    Staged(Box<ThreadTable>),
    Published(ThreadTableCapability),
    Publishing,
}

impl ThreadTableStorage {
    fn access(&self) -> ThreadTableAccess<'_> {
        match self {
            Self::Staged(table) => ThreadTableAccess::Staged(table),
            Self::Published(capability) => ThreadTableAccess::Published(*capability),
            Self::Publishing => registry_invariant(),
        }
    }
}

struct ThreadTableAuthorityToken {
    _private: (),
}

/// Closure-bounded read access to address-stable Thread records.
pub(super) struct ThreadTableReadAuthority<'table> {
    table: ThreadTableAccess<'table>,
    _token: &'table ThreadTableAuthorityToken,
}

impl ThreadTableReadAuthority<'_> {
    pub fn with_thread<R>(
        &self,
        id: ThreadId,
        operation: impl for<'thread> FnOnce(&'thread Thread) -> R,
    ) -> Result<R, Error> {
        let slot = id.scheduler_slot().ok_or(Error::ThreadNotFound)?;
        match self.table.table().slot(slot) {
            Some(ThreadSlot::Occupied(thread)) if thread.id() == id => Ok(operation(thread)),
            _ => Err(Error::ThreadNotFound),
        }
    }
}

/// Exclusive closure-bounded access to address-stable Thread records.
///
/// The invariant marker prevents accidentally weakening this authority to a
/// shared table borrow when it is later moved into per-CPU scheduler domains.
pub(super) struct ThreadTableWriteAuthority<'table> {
    table: ThreadTableAccess<'table>,
    _token: &'table mut ThreadTableAuthorityToken,
    _exclusive: PhantomData<&'table mut ThreadSlot>,
}

/// Authority over waiting/terminated topology, independent of CPU schedules.
///
/// Exclusive coordination protects lifecycle operations. Shared wait operations
/// instead retain a registry reader lane and the affected queue lock. Either
/// authority can update adjacent links without borrowing their CPU schedules.
pub(super) struct ThreadControlAuthority<'table> {
    table: ThreadTableAccess<'table>,
    _token: &'table ThreadTableAuthorityToken,
    _exclusive: PhantomData<&'table mut ThreadSlot>,
}

impl ThreadControlAuthority<'_> {
    pub(super) fn links(&self, id: ThreadId) -> Result<crate::task::thread::QueueLinks, Error> {
        let slot = id.scheduler_slot().ok_or(Error::ThreadNotFound)?;
        match self.table.table().slot(slot) {
            Some(ThreadSlot::Occupied(thread)) if thread.id() == id => {
                // SAFETY: this authority holds exclusive coordination or
                // was minted under the affected queue lock and reader lane.
                Ok(unsafe { thread.control_queue_links() })
            }
            _ => Err(Error::ThreadNotFound),
        }
    }

    pub(super) fn with_links_mut<R>(
        &mut self,
        id: ThreadId,
        operation: impl FnOnce(&mut crate::task::thread::QueueLinks) -> R,
    ) -> Result<R, Error> {
        let slot = id.scheduler_slot().ok_or(Error::ThreadNotFound)?;
        match self.table.table().slot(slot) {
            Some(ThreadSlot::Occupied(thread)) if thread.id() == id => {
                // SAFETY: exclusive coordination or the matching queue lock
                // serializes these links, and registry protection pins the slot.
                Ok(unsafe { thread.with_control_queue_links_mut(operation) })
            }
            _ => Err(Error::ThreadNotFound),
        }
    }
}

impl ThreadTableWriteAuthority<'_> {
    pub fn with_thread<R>(
        &self,
        id: ThreadId,
        operation: impl for<'thread> FnOnce(&'thread Thread) -> R,
    ) -> Result<R, Error> {
        let slot = id.scheduler_slot().ok_or(Error::ThreadNotFound)?;
        match self.table.table().slot(slot) {
            Some(ThreadSlot::Occupied(thread)) if thread.id() == id => Ok(operation(thread)),
            _ => Err(Error::ThreadNotFound),
        }
    }

    pub fn with_thread_mut<R>(
        &mut self,
        id: ThreadId,
        operation: impl for<'thread> FnOnce(&'thread mut Thread) -> R,
    ) -> Result<R, Error> {
        let slot = id.scheduler_slot().ok_or(Error::ThreadNotFound)?;
        match self.table.table().slot_mut(slot, &mut *self._token) {
            Some(ThreadSlot::Occupied(thread)) if thread.id() == id => Ok(operation(thread)),
            _ => Err(Error::ThreadNotFound),
        }
    }

    pub(super) fn control_links(
        &self,
        id: ThreadId,
    ) -> Result<crate::task::thread::QueueLinks, Error> {
        self.with_thread(id, |thread| {
            // SAFETY: write authority is minted only under TransitionLock,
            // which exclusively owns the independent control-link domain.
            unsafe { thread.control_queue_links() }
        })
    }
}

/// Linear ownership of one unpublished registry slot.
#[must_use = "a thread reservation must be published or explicitly abandoned"]
pub(super) struct ThreadReservation {
    id: ThreadId,
    slot: usize,
    cpu: CpuIndex,
    armed: bool,
}

// A reservation is intentionally transferable: it owns no CPU-local machine
// state, and `cpu` is placement data rather than an execution-affinity proof.
// Every consume operation revalidates its complete ThreadId under SCHEDULER.

impl ThreadReservation {
    pub const fn id(&self) -> ThreadId {
        self.id
    }

    pub const fn cpu(&self) -> CpuIndex {
        self.cpu
    }

    pub fn disarm(&mut self) {
        if !self.armed {
            hyper::debug::invariant_failure("task::scheduler::registry::disarm invariant")
        }
        self.armed = false;
    }
}

impl Drop for ThreadReservation {
    fn drop(&mut self) {
        if self.armed {
            hyper::debug::invariant_failure("task::scheduler::registry::drop invariant")
        }
    }
}

/// Growable scheduler registry with never-reused public identities.
pub(super) struct ThreadRegistry {
    table: ThreadTableStorage,
    table_authority: ThreadTableAuthorityToken,
    free_head: Option<usize>,
    high_water: usize,
    next_identity: u64,
}

impl ThreadRegistry {
    /// Creates topology authority while a registry reader lane pins all slots.
    ///
    /// # Safety
    ///
    /// The caller must hold the affected `WaitQueue` lock throughout use. All
    /// referenced nodes must belong to that queue, except an insertion/removal
    /// candidate whose CPU scheduler lock the caller also holds.
    pub(super) unsafe fn wait_control_authority(&self) -> ThreadControlAuthority<'_> {
        ThreadControlAuthority {
            table: self.table.access(),
            _token: &self.table_authority,
            _exclusive: PhantomData,
        }
    }

    pub fn new(bootstrap: Box<Thread>, idle: Box<Thread>) -> Result<Self, Error> {
        let mut growth = TableGrowth::prepare(0)?;
        if bootstrap.id() != ThreadId::BOOTSTRAP
            || bootstrap.id().scheduler_slot() != Some(0)
            || idle.id().scheduler_slot() != Some(1)
        {
            return Err(Error::InvalidThreadState);
        }
        let cells = growth
            .cells
            .as_mut()
            .unwrap_or_else(|| registry_invariant());
        *cells[0].0.get_mut() = ThreadSlot::Occupied(bootstrap);
        *cells[1].0.get_mut() = ThreadSlot::Occupied(idle);
        let table = hyper::mm::try_box(ThreadTable {
            segments: [const { AtomicPtr::new(core::ptr::null_mut()) }; SEGMENTS],
        })
        .map_err(|_| Error::Allocation)?;
        table.install(&mut growth, &mut ThreadTableAuthorityToken { _private: () });
        Ok(Self {
            table: ThreadTableStorage::Staged(table),
            table_authority: ThreadTableAuthorityToken { _private: () },
            free_head: None,
            high_water: 2,
            next_identity: 2,
        })
    }

    pub fn reserve(&mut self, cpu: CpuIndex) -> Result<ThreadReservation, Error> {
        let slot = if let Some(slot) = self.free_head {
            let Some(ThreadSlot::Free(next)) = self.table.access().table().slot(slot) else {
                registry_invariant();
            };
            self.free_head = *next;
            *self
                .table_slot_mut(slot)
                .unwrap_or_else(|| registry_invariant()) = ThreadSlot::Vacant;
            slot
        } else {
            let slot = self.high_water;
            if ThreadId::from_scheduler_parts(self.next_identity, slot).is_none() {
                return Err(Error::IdentifierExhausted);
            }
            if self.table.access().table().slot(slot).is_none() {
                return Err(Error::ThreadTableGrowth);
            }
            self.high_water += 1;
            slot
        };

        let Some(id) = ThreadId::from_scheduler_parts(self.next_identity, slot) else {
            self.release_vacant(slot)?;
            return Err(Error::IdentifierExhausted);
        };
        let Some(next_identity) = self.next_identity.checked_add(1) else {
            self.release_vacant(slot)?;
            return Err(Error::IdentifierExhausted);
        };
        self.next_identity = next_identity;
        *self
            .table_slot_mut(slot)
            .unwrap_or_else(|| registry_invariant()) = ThreadSlot::Reserved(id);
        Ok(ThreadReservation {
            id,
            slot,
            cpu,
            armed: true,
        })
    }

    pub fn publish(
        &mut self,
        reservation: &ThreadReservation,
        thread: Box<Thread>,
    ) -> Result<(), (Error, Box<Thread>)> {
        if thread.id() != reservation.id
            || thread.cpu_index() != reservation.cpu
            || !matches!(
                self.table.access().table().slot(reservation.slot),
                Some(ThreadSlot::Reserved(id)) if *id == reservation.id
            )
        {
            return Err((Error::InvalidThreadState, thread));
        }
        *self
            .table_slot_mut(reservation.slot)
            .unwrap_or_else(|| registry_invariant()) = ThreadSlot::Occupied(thread);
        Ok(())
    }

    pub fn abandon(&mut self, reservation: &ThreadReservation) -> Result<(), Error> {
        if !matches!(
            self.table.access().table().slot(reservation.slot),
            Some(ThreadSlot::Reserved(id)) if *id == reservation.id
        ) {
            return Err(Error::InvalidThreadState);
        }
        self.preflight_release(reservation.slot)?;
        *self
            .table_slot_mut(reservation.slot)
            .unwrap_or_else(|| registry_invariant()) = ThreadSlot::Vacant;
        self.commit_release(reservation.slot);
        Ok(())
    }

    pub fn take(&mut self, id: ThreadId) -> Result<Box<Thread>, Error> {
        let slot = id.scheduler_slot().ok_or(Error::ThreadNotFound)?;
        if !matches!(
            self.table.access().table().slot(slot),
            Some(ThreadSlot::Occupied(thread)) if thread.id() == id
        ) {
            return Err(Error::ThreadNotFound);
        }
        if matches!(
            self.table.access().table().slot(slot),
            Some(ThreadSlot::Occupied(thread)) if !thread.schedule_is_coordinator_owned()
        ) {
            return Err(Error::InvalidThreadState);
        }
        // The bootstrap object is retired after kernel initialization, but
        // slot zero is its permanent identity and must never enter the reusable
        // namespace. Every other detached slot is returned to the free list.
        if slot != 0 {
            self.preflight_release(slot)?;
        }
        let slot_ref = self
            .table_slot_mut(slot)
            .unwrap_or_else(|| registry_invariant());
        let ThreadSlot::Occupied(thread) = core::mem::replace(slot_ref, ThreadSlot::Vacant) else {
            registry_invariant();
        };
        if slot != 0 {
            self.commit_release(slot);
        }
        Ok(thread)
    }

    /// Detaches one stopped Thread while retaining its slot as an ABA barrier.
    ///
    /// The allocation stays in the stable slot until the dedicated reaper
    /// takes it. The empty Retiring marker then remains until destruction and
    /// terminal publication have both completed.
    pub fn begin_retirement(&mut self, id: ThreadId) -> Result<(), Error> {
        let slot = id.scheduler_slot().ok_or(Error::ThreadNotFound)?;
        if !matches!(
            self.table.access().table().slot(slot),
            Some(ThreadSlot::Occupied(thread))
                if thread.id() == id && thread.schedule_is_coordinator_owned()
        ) {
            return Err(Error::InvalidThreadState);
        }
        let slot_ref = self
            .table_slot_mut(slot)
            .unwrap_or_else(|| registry_invariant());
        let ThreadSlot::Occupied(thread) = core::mem::replace(slot_ref, ThreadSlot::Vacant) else {
            registry_invariant();
        };
        let object = thread.object_snapshot();
        let name = thread.name_snapshot();
        let runtime_ns = thread.runtime_ns();
        *slot_ref = ThreadSlot::Retiring {
            id,
            name,
            object,
            runtime_ns,
            thread: Some(thread),
            retirement: Some(super::ResourceRetirement::begin()),
            next: None,
        };
        Ok(())
    }

    pub fn link_retirement(&mut self, tail: ThreadId, next_id: ThreadId) -> Result<(), Error> {
        let slot = tail.scheduler_slot().ok_or(Error::ThreadNotFound)?;
        match self.table_slot_mut(slot) {
            Some(ThreadSlot::Retiring {
                id,
                next,
                thread: Some(_),
                ..
            }) if *id == tail && next.is_none() => {
                *next = Some(next_id);
                Ok(())
            }
            _ => Err(Error::InvalidThreadState),
        }
    }

    /// Transfers teardown ownership, retaining the slot until destruction ends.
    pub fn take_retiring(
        &mut self,
        id: ThreadId,
    ) -> Result<(super::state::RetiredThread, Option<ThreadId>), Error> {
        let slot = id.scheduler_slot().ok_or(Error::ThreadNotFound)?;
        match self.table_slot_mut(slot) {
            Some(ThreadSlot::Retiring {
                id: retiring,
                thread,
                retirement,
                next,
                ..
            }) if *retiring == id && thread.is_some() && retirement.is_some() => Ok((
                super::state::RetiredThread {
                    id,
                    thread: thread.take().unwrap_or_else(|| registry_invariant()),
                    retirement: retirement.take().unwrap_or_else(|| registry_invariant()),
                },
                next.take(),
            )),
            _ => Err(Error::InvalidThreadState),
        }
    }

    /// Releases a slot only after every detached resource and publication ends.
    pub fn complete_retirement(&mut self, id: ThreadId) -> Result<(), Error> {
        let slot = id.scheduler_slot().ok_or(Error::ThreadNotFound)?;
        if !matches!(
            self.table.access().table().slot(slot),
            Some(ThreadSlot::Retiring {
                id: retiring,
                thread: None,
                retirement: None,
                next: None,
                ..
            }) if *retiring == id
        ) {
            return Err(Error::InvalidThreadState);
        }
        // Bootstrap may legitimately terminate after handing execution to the
        // first long-lived workload. Its reserved slot zero becomes Vacant
        // for observation, but is never admitted to the reusable free list.
        if slot != 0 {
            self.preflight_release(slot)?;
        }
        *self
            .table_slot_mut(slot)
            .unwrap_or_else(|| registry_invariant()) = ThreadSlot::Vacant;
        if slot != 0 {
            self.commit_release(slot);
        }
        Ok(())
    }

    pub fn read_authority(&self) -> ThreadTableReadAuthority<'_> {
        ThreadTableReadAuthority {
            table: self.table.access(),
            _token: &self.table_authority,
        }
    }

    pub fn write_authority(&mut self) -> ThreadTableWriteAuthority<'_> {
        let table = self.table.access();
        ThreadTableWriteAuthority {
            table,
            _token: &mut self.table_authority,
            _exclusive: PhantomData,
        }
    }

    pub(super) fn control_authority(&mut self) -> ThreadControlAuthority<'_> {
        let table = self.table.access();
        ThreadControlAuthority {
            table,
            _token: &mut self.table_authority,
            _exclusive: PhantomData,
        }
    }

    pub fn with_thread<R>(
        &self,
        id: ThreadId,
        operation: impl for<'thread> FnOnce(&'thread Thread) -> R,
    ) -> Result<R, Error> {
        self.read_authority().with_thread(id, operation)
    }

    pub fn with_thread_mut<R>(
        &mut self,
        id: ThreadId,
        operation: impl for<'thread> FnOnce(&'thread mut Thread) -> R,
    ) -> Result<R, Error> {
        self.write_authority().with_thread_mut(id, operation)
    }

    /// Returns canonical object identity for an occupied or retiring Thread.
    #[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
    pub fn object_snapshot(
        &self,
        id: ThreadId,
    ) -> Result<crate::task::ThreadObjectSnapshot, Error> {
        let slot = id.scheduler_slot().ok_or(Error::ThreadNotFound)?;
        let table = self.table.access().table();
        match table.slot(slot) {
            Some(ThreadSlot::Occupied(thread)) if thread.id() == id => Ok(thread.object_snapshot()),
            Some(ThreadSlot::Retiring {
                id: retiring,
                object,
                ..
            }) if *retiring == id => Ok(*object),
            _ => Err(Error::ThreadNotFound),
        }
    }

    /// Captures one bounded page without allowing Thread or object references
    /// to escape the scheduler authority domain.
    pub fn scan_objects(
        &self,
        cursor: crate::task::ThreadObjectScanCursor,
        output: &mut crate::task::ThreadObjectSnapshotPage,
    ) {
        use {
            crate::task::ThreadObjectObservation, crate::task::ThreadObjectRegistryPhase,
            crate::task::ThreadObjectScanCursor,
        };

        output.len = 0;
        output.next = None;
        let mut index = cursor.next_slot;
        let table = self.table.access().table();
        while index < self.high_water && output.len < output.entries.len() {
            let observation = match table.slot(index) {
                Some(ThreadSlot::Occupied(thread)) => Some(ThreadObjectObservation {
                    thread: thread.id(),
                    name: thread.name_snapshot(),
                    object: thread.object_snapshot(),
                    phase: ThreadObjectRegistryPhase::Resident,
                    runtime_ns: thread.runtime_ns(),
                }),
                Some(ThreadSlot::Retiring {
                    id,
                    name,
                    object,
                    runtime_ns,
                    ..
                }) => Some(ThreadObjectObservation {
                    thread: *id,
                    name: *name,
                    object: *object,
                    phase: ThreadObjectRegistryPhase::Retiring,
                    runtime_ns: *runtime_ns,
                }),
                Some(ThreadSlot::Vacant | ThreadSlot::Free(_) | ThreadSlot::Reserved(_)) | None => {
                    None
                }
            };
            if let Some(observation) = observation {
                output.entries[output.len] = Some(observation);
                output.len += 1;
            }
            index += 1;
        }
        output.next =
            (index < self.high_water).then_some(ThreadObjectScanCursor { next_slot: index });
    }

    pub fn for_each_thread(&self, mut operation: impl for<'thread> FnMut(&'thread Thread)) {
        for index in 0..self.high_water {
            if let Some(ThreadSlot::Occupied(thread)) = self.table.access().table().slot(index) {
                operation(thread);
            }
        }
    }

    /// Visits one slot from a coordinator-owned, finite balancing sweep.
    /// Only an identity escapes; exclusive coordination retains its lifetime.
    pub(super) fn balance_candidate(&self, index: usize) -> Option<ThreadId> {
        match self.table.access().table().slot(index) {
            Some(ThreadSlot::Occupied(thread)) => Some(thread.id()),
            _ => None,
        }
    }

    pub(super) const fn balance_slot_count(&self) -> usize {
        self.high_water
    }

    /// Number of slots ever exposed by the monotonic backing high-water mark.
    #[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
    pub const fn high_water(&self) -> usize {
        self.high_water
    }

    fn release_vacant(&mut self, slot: usize) -> Result<(), Error> {
        if !matches!(
            self.table.access().table().slot(slot),
            Some(ThreadSlot::Vacant)
        ) {
            return Err(Error::InvalidThreadState);
        }
        self.preflight_release(slot)?;
        self.commit_release(slot);
        Ok(())
    }

    fn preflight_release(&self, slot: usize) -> Result<(), Error> {
        if slot == 0
            || slot >= self.high_water
            || matches!(
                self.table.access().table().slot(slot),
                Some(ThreadSlot::Free(_))
            )
        {
            return Err(Error::InvalidThreadState);
        }
        Ok(())
    }

    fn commit_release(&mut self, slot: usize) {
        let next = self.free_head;
        *self
            .table_slot_mut(slot)
            .unwrap_or_else(|| registry_invariant()) = ThreadSlot::Free(next);
        self.free_head = Some(slot);
    }

    pub fn growth_needed(&self) -> Option<usize> {
        (self.free_head.is_none() && self.table.access().table().slot(self.high_water).is_none())
            .then(|| segment_for(self.high_water))
    }

    pub fn install_growth(&mut self, growth: &mut TableGrowth) {
        self.table
            .access()
            .table()
            .install(growth, &mut self.table_authority);
    }

    fn table_slot_mut(&mut self, slot: usize) -> Option<&mut ThreadSlot> {
        let table = self.table.access();
        table.table().slot_mut(slot, &mut self.table_authority)
    }

    /// Permanently publishes the stable directory after every fallible scheduler
    /// construction step has completed.
    pub fn publish_table(&mut self) {
        let storage = core::mem::replace(&mut self.table, ThreadTableStorage::Publishing);
        let ThreadTableStorage::Staged(table) = storage else {
            registry_invariant();
        };
        let table = Box::leak(table);
        self.table = ThreadTableStorage::Published(ThreadTableCapability { table });
    }

    pub(super) fn published_table(&self) -> ThreadTableCapability {
        match self.table {
            ThreadTableStorage::Published(capability) => capability,
            ThreadTableStorage::Staged(_) | ThreadTableStorage::Publishing => registry_invariant(),
        }
    }
}

fn registry_invariant() -> ! {
    // Registry corruption is observed while the scheduler lock is held. Do
    // not use ordinary diagnostics that reacquire scheduler-adjacent locks.
    hyper::debug::invariant_failure("task::scheduler::registry::registry_invariant invariant")
}
