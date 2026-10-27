// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Hardware translation tags with software ownership and lazy-flush epochs.
//!
//! The caller serializes this pool and flushes each CPU before consuming a new
//! epoch. Leases pin tags while hardware can still use them. Rollover preserves
//! pinned tags; it never requires a synchronous all-CPU rendezvous. Zero is
//! reserved. Segment preparation and destruction belong outside caller locks.

use alloc::boxed::Box;
use core::marker::PhantomData;
use core::sync::atomic::{AtomicU16, Ordering};

#[cfg(not(test))]
use super::try_box_uninit;
#[cfg(test)]
use hyper::mm::try_box_uninit;

const SEGMENT_SIZE: usize = 256;
const SEGMENTS: usize = (u16::MAX as usize + 1) / SEGMENT_SIZE;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TranslationEpochError {
    Allocation,
    InvalidWidth,
    WidthChanged,
    Overflow,
    Busy,
    InvalidToken,
}

/// Validated architectural tag width. The remaining u64 bits hold the epoch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TranslationTagWidth(u8);

impl TranslationTagWidth {
    pub const fn new(bits: u8) -> Result<Self, TranslationEpochError> {
        if bits == 0 || bits > 16 {
            Err(TranslationEpochError::InvalidWidth)
        } else {
            Ok(Self(bits))
        }
    }

    pub const fn bits(self) -> u8 {
        self.0
    }
    pub const fn id_mask(self) -> u64 {
        (1u64 << self.0) - 1
    }
    pub const fn max_epoch(self) -> u64 {
        u64::MAX >> self.0
    }

    /// Encodes one nonzero hardware tag and one nonzero allocation epoch.
    pub const fn pack(self, id: u16, epoch: u64) -> Result<TranslationTag, TranslationEpochError> {
        if id == 0 || id as u64 > self.id_mask() || epoch == 0 {
            return Err(TranslationEpochError::InvalidToken);
        }
        if epoch > self.max_epoch() {
            return Err(TranslationEpochError::Overflow);
        }
        Ok(TranslationTag((epoch << self.0) | id as u64))
    }

    /// Validates a stored value using its owning namespace's admitted width.
    pub const fn decode(self, raw: u64) -> Result<TranslationTag, TranslationEpochError> {
        self.pack((raw & self.id_mask()) as u16, raw >> self.0)
    }
}

/// Packed allocation identity: hardware ID in the low width bits, epoch above.
/// This value is not a pin; only a live `TranslationLease` authorizes hardware use.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(transparent)]
pub struct TranslationTag(u64);

impl TranslationTag {
    pub const fn value(self) -> u64 {
        self.0
    }

    // Decoding is private so public consumers cannot substitute another
    // namespace's width. Lease construction retains the validated descriptor.
    const fn id(self, width: TranslationTagWidth) -> u16 {
        (self.0 & width.id_mask()) as u16
    }
    const fn epoch(self, width: TranslationTagWidth) -> u64 {
        self.0 >> width.bits()
    }
}

#[derive(Clone, Copy)]
struct Slot {
    owner: u64,
    pins: usize,
}

impl Slot {
    const EMPTY: Self = Self { owner: 0, pins: 0 };
}

/// Allocator-owned storage prepared before taking the pool's external lock.
pub struct TranslationEpochSegment {
    slots: [Slot; SEGMENT_SIZE],
    free: [u64; SEGMENT_SIZE / 64],
}

impl TranslationEpochSegment {
    fn reset_free(&mut self, index: usize, width: u8) {
        let slots = (1usize << width)
            .saturating_sub(index * SEGMENT_SIZE)
            .min(SEGMENT_SIZE);
        for (word, bits) in self.free.iter_mut().enumerate() {
            let count = slots.saturating_sub(word * 64).min(64);
            *bits = if count == 64 {
                u64::MAX
            } else {
                (1u64 << count) - 1
            };
        }
        if index == 0 {
            self.free[0] &= !1;
        }
    }

    // Segment expansion is a cold path, but inlining its old by-value array
    // construction reserved 4 KiB throughout Native process/VM creation.
    #[inline(never)]
    pub fn try_new() -> Result<Box<Self>, TranslationEpochError> {
        let mut segment =
            try_box_uninit::<Self>().map_err(|_| TranslationEpochError::Allocation)?;
        let pointer = segment.as_mut_ptr();
        // SAFETY: The Box exclusively owns aligned storage for Self. Initialize
        // every slot and the free bitmap in that allocation before exposing a
        // reference to Self. No fallible operation or destructor intervenes.
        unsafe {
            let slots = core::ptr::addr_of_mut!((*pointer).slots).cast::<Slot>();
            for index in 0..SEGMENT_SIZE {
                slots.add(index).write(Slot::EMPTY);
            }
            core::ptr::addr_of_mut!((*pointer).free).write([u64::MAX; SEGMENT_SIZE / 64]);
            Ok(segment.assume_init())
        }
    }
}

/// A linear software identity; hardware-tag exhaustion never prevents creation.
#[must_use = "registered software ownership must be explicitly unregistered"]
pub struct TranslationOwner<Namespace> {
    serial: u64,
    // A hint only. The externally serialized pool is its sole writer; atomics
    // keep software owners shareable without an additional per-owner lock.
    binding: AtomicU16,
    namespace: PhantomData<fn() -> Namespace>,
}

impl<Namespace> TranslationOwner<Namespace> {
    pub const fn serial(&self) -> u64 {
        self.serial
    }

    pub const fn binding_hint(&self, id: u16) -> TranslationBinding {
        TranslationBinding {
            id,
            owner: self.serial,
        }
    }
}

impl<Namespace> core::fmt::Debug for TranslationOwner<Namespace> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_tuple("TranslationOwner")
            .field(&self.serial)
            .finish()
    }
}

/// A cache hint, not an execution capability. The pool validates every hint.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TranslationBinding {
    id: u16,
    owner: u64,
}

impl TranslationBinding {
    pub const fn id(self) -> u16 {
        self.id
    }
    pub const fn owner(self) -> u64 {
        self.owner
    }
}

/// Exact active pin. It can be released after any number of intervening epochs.
#[must_use = "hardware use must end before releasing its translation lease"]
pub struct TranslationLease<Namespace> {
    tag: TranslationTag,
    width: TranslationTagWidth,
    owner: u64,
    namespace: PhantomData<fn() -> Namespace>,
}

impl<Namespace> TranslationLease<Namespace> {
    pub const fn tag(&self) -> TranslationTag {
        self.tag
    }
    pub const fn binding(&self) -> TranslationBinding {
        TranslationBinding {
            id: self.tag.id(self.width),
            owner: self.owner,
        }
    }
    pub const fn epoch(&self) -> u64 {
        self.tag.epoch(self.width)
    }
}

impl<Namespace> core::fmt::Debug for TranslationLease<Namespace> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("TranslationLease")
            .field("tag", &self.tag)
            .field("width", &self.width)
            .field("owner", &self.owner)
            .finish()
    }
}

/// No internal synchronization, allocation, TLB maintenance, or CPU callbacks.
pub struct TranslationEpochPool<Namespace> {
    segments: [Option<Box<TranslationEpochSegment>>; SEGMENTS],
    segment_count: usize,
    width: Option<TranslationTagWidth>,
    serial: u64,
    live: usize,
    epoch: u64,
    namespace: PhantomData<fn() -> Namespace>,
}

impl<Namespace> TranslationEpochPool<Namespace> {
    /// # Safety
    /// This must be the unique live pool for `Namespace`. No owner, binding or
    /// lease from an earlier pool may be used with this one.
    pub const unsafe fn new() -> Self {
        Self {
            segments: [const { None }; SEGMENTS],
            segment_count: 0,
            width: None,
            serial: 0,
            live: 0,
            epoch: 1,
            namespace: PhantomData,
        }
    }

    pub const fn epoch(&self) -> u64 {
        self.epoch
    }
    pub const fn live_owners(&self) -> usize {
        self.live
    }

    /// Registers without allocating. `None` requests one prepared segment and
    /// changes nothing, including width, software serial, and spare ownership.
    pub fn register_owner(
        &mut self,
        width: u8,
        spare: &mut Option<Box<TranslationEpochSegment>>,
    ) -> Result<Option<TranslationOwner<Namespace>>, TranslationEpochError> {
        let width = TranslationTagWidth::new(width)?;
        if self.width.is_some_and(|installed| installed != width) {
            return Err(TranslationEpochError::WidthChanged);
        }
        let serial = self
            .serial
            .checked_add(1)
            .ok_or(TranslationEpochError::Overflow)?;
        let live = self
            .live
            .checked_add(1)
            .ok_or(TranslationEpochError::Overflow)?;
        let needed = required_segments(live, width);
        if needed > self.segment_count {
            let Some(mut segment) = spare.take() else {
                return Ok(None);
            };
            // Detached storage may originate in another pool. No tag history
            // transfers with the allocation; removal advanced that pool's epoch.
            segment.slots.fill(Slot::EMPTY);
            segment.reset_free(self.segment_count, width.bits());
            self.segments[self.segment_count] = Some(segment);
            self.segment_count += 1;
        }
        self.width = Some(width);
        self.serial = serial;
        self.live = live;
        Ok(Some(TranslationOwner {
            serial,
            binding: AtomicU16::new(0),
            namespace: PhantomData,
        }))
    }

    /// Pins a cached tag or chooses a fresh one. The caller must observe the
    /// returned epoch and complete its CPU's lazy flush before using the tag.
    pub fn acquire(
        &mut self,
        owner: &TranslationOwner<Namespace>,
        hint: Option<TranslationBinding>,
    ) -> Result<TranslationLease<Namespace>, TranslationEpochError> {
        self.validate_owner(owner)?;
        let width = self.width.ok_or(TranslationEpochError::InvalidWidth)?;
        // Validate the owner's current cache first. Caller hints can lag a
        // concurrent acquisition and must not manufacture a second binding.
        let cached = owner.binding.load(Ordering::Relaxed);
        let existing = [Some(owner.binding_hint(cached)), hint]
            .into_iter()
            .flatten()
            .filter(|binding| binding.id != 0 && binding.owner == owner.serial)
            .find_map(|binding| {
                self.slot(binding.id as usize)
                    .filter(|slot| slot.owner == owner.serial)
                    .map(|_| binding.id as usize)
            });
        let id = match existing {
            Some(id) => id,
            None => match self.free_slot() {
                Some(id) => id,
                None => {
                    self.rollover()?;
                    self.free_slot().ok_or(TranslationEpochError::Busy)?
                }
            },
        };
        let tag = width.pack(id as u16, self.epoch)?;
        let slot = self
            .slot_mut(id)
            .ok_or(TranslationEpochError::InvalidToken)?;
        let pins = slot
            .pins
            .checked_add(1)
            .ok_or(TranslationEpochError::Overflow)?;
        slot.owner = owner.serial;
        slot.pins = pins;
        self.mark_used(id);
        owner.binding.store(id as u16, Ordering::Relaxed);
        Ok(TranslationLease {
            tag,
            width,
            owner: owner.serial,
            namespace: PhantomData,
        })
    }

    /// Failure returns the exact retained pin; old epochs remain releasable.
    pub fn release(
        &mut self,
        lease: TranslationLease<Namespace>,
    ) -> Result<(), (TranslationEpochError, TranslationLease<Namespace>)> {
        let Some(slot) = self.slot_mut(lease.binding().id as usize) else {
            return Err((TranslationEpochError::InvalidToken, lease));
        };
        if lease.binding().id == 0 || slot.owner != lease.owner || slot.pins == 0 {
            return Err((TranslationEpochError::InvalidToken, lease));
        }
        slot.pins -= 1;
        Ok(())
    }

    /// A busy failure retains the software owner for retry after all pins leave.
    pub fn unregister_owner(
        &mut self,
        owner: TranslationOwner<Namespace>,
    ) -> Result<(), (TranslationEpochError, TranslationOwner<Namespace>)> {
        if let Err(error) = self.validate_owner(&owner) {
            return Err((error, owner));
        }
        let cached = owner.binding.load(Ordering::Relaxed) as usize;
        if let Some(slot) = self.slot_mut(cached)
            && slot.owner == owner.serial
        {
            if slot.pins != 0 {
                return Err((TranslationEpochError::Busy, owner));
            }
            slot.owner = 0;
            // Keep the free bit clear as a tombstone until lazy-flush rollover.
        }
        self.live -= 1;
        Ok(())
    }

    /// Detaches the highest unnecessary, unpinned segment for destruction after
    /// unlocking. Advancing the epoch first preserves tag-reuse history even
    /// if the same segment is later allocated again. Overflow changes nothing.
    pub fn take_unused_segment(&mut self) -> Option<Box<TranslationEpochSegment>> {
        let width = self.width?;
        if self.segment_count <= required_segments(self.live, width) {
            return None;
        }
        let index = self.segment_count - 1;
        if self.segments[index]
            .as_ref()?
            .slots
            .iter()
            .any(|slot| slot.pins != 0)
        {
            return None;
        }
        self.advance_epoch().ok()?;
        self.segment_count -= 1;
        self.segments[index].take()
    }

    fn validate_owner(
        &self,
        owner: &TranslationOwner<Namespace>,
    ) -> Result<(), TranslationEpochError> {
        // Linear construction/consumption and the unique-pool contract rule
        // out already-unregistered owners without a second software registry.
        if owner.serial == 0 || owner.serial > self.serial || self.live == 0 {
            return Err(TranslationEpochError::InvalidToken);
        }
        Ok(())
    }

    fn capacity(&self) -> usize {
        self.width.map_or(0, |width| {
            (self.segment_count * SEGMENT_SIZE).min(1usize << width.bits())
        })
    }

    fn slot(&self, id: usize) -> Option<&Slot> {
        if id >= self.capacity() {
            return None;
        }
        self.segments[id / SEGMENT_SIZE]
            .as_ref()
            .map(|segment| &segment.slots[id % SEGMENT_SIZE])
    }

    fn slot_mut(&mut self, id: usize) -> Option<&mut Slot> {
        if id >= self.capacity() {
            return None;
        }
        self.segments[id / SEGMENT_SIZE]
            .as_mut()
            .map(|segment| &mut segment.slots[id % SEGMENT_SIZE])
    }

    fn mark_used(&mut self, id: usize) {
        if let Some(segment) = self.segments[id / SEGMENT_SIZE].as_mut() {
            let slot = id % SEGMENT_SIZE;
            segment.free[slot / 64] &= !(1u64 << (slot % 64));
        }
    }

    fn free_slot(&self) -> Option<usize> {
        for (index, segment) in self.segments.iter().enumerate().take(self.segment_count) {
            if let Some(segment) = segment {
                for (word, free) in segment.free.iter().enumerate() {
                    if *free != 0 {
                        return Some(
                            index * SEGMENT_SIZE + word * 64 + free.trailing_zeros() as usize,
                        );
                    }
                }
            }
        }
        None
    }

    fn rollover(&mut self) -> Result<(), TranslationEpochError> {
        if !(1..self.capacity()).any(|id| self.slot(id).is_some_and(|slot| slot.pins == 0)) {
            return Err(TranslationEpochError::Busy);
        }
        self.advance_epoch()
    }

    fn advance_epoch(&mut self) -> Result<(), TranslationEpochError> {
        let width = self.width.ok_or(TranslationEpochError::InvalidWidth)?;
        let next = self
            .epoch
            .checked_add(1)
            .filter(|epoch| *epoch <= width.max_epoch())
            .ok_or(TranslationEpochError::Overflow)?;
        for (index, segment) in self.segments.iter_mut().enumerate() {
            if let Some(segment) = segment {
                segment.reset_free(index, width.bits());
                for (slot_index, slot) in segment.slots.iter_mut().enumerate() {
                    if slot.pins == 0 {
                        *slot = Slot::EMPTY;
                    } else {
                        segment.free[slot_index / 64] &= !(1u64 << (slot_index % 64));
                    }
                }
            }
        }
        self.epoch = next;
        Ok(())
    }
}

fn required_segments(live: usize, width: TranslationTagWidth) -> usize {
    if live == 0 {
        0
    } else {
        (live.min(width.id_mask() as usize) + 1).div_ceil(SEGMENT_SIZE)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    enum TestNamespace {}
    type Pool = TranslationEpochPool<TestNamespace>;
    type Owner = TranslationOwner<TestNamespace>;

    fn pool() -> Pool {
        // SAFETY: Each test owns an isolated pool; tokens never cross tests.
        unsafe { Pool::new() }
    }

    fn ok<T, E: core::fmt::Debug>(result: Result<T, E>) -> T {
        match result {
            Ok(value) => value,
            Err(error) => panic!("unexpected error: {error:?}"),
        }
    }

    fn register(pool: &mut Pool, width: u8) -> Owner {
        let mut spare = None;
        if let Some(owner) = ok(pool.register_owner(width, &mut spare)) {
            return owner;
        }
        spare = Some(ok(TranslationEpochSegment::try_new()));
        match ok(pool.register_owner(width, &mut spare)) {
            Some(owner) => owner,
            None => panic!("one prepared segment did not satisfy registration"),
        }
    }

    #[test]
    fn packed_tag_round_trips_supported_widths_and_rejects_reserved_values() {
        assert_eq!(core::mem::size_of::<TranslationTag>(), 8);
        for bits in [1, 8, 14, 16] {
            let width = ok(TranslationTagWidth::new(bits));
            for (id, epoch) in [
                (1, 1),
                (width.id_mask() as u16, 7),
                (width.id_mask() as u16, width.max_epoch()),
            ] {
                let tag = ok(width.pack(id, epoch));
                assert_eq!(tag.value(), (epoch << bits) | id as u64);
                assert_eq!(tag.id(width), id);
                assert_eq!(tag.epoch(width), epoch);
                assert_eq!(ok(width.decode(tag.value())), tag);
            }
            assert_eq!(
                ok(width.pack(width.id_mask() as u16, width.max_epoch())).value(),
                u64::MAX
            );
            assert_eq!(width.pack(0, 1), Err(TranslationEpochError::InvalidToken));
            assert_eq!(width.pack(1, 0), Err(TranslationEpochError::InvalidToken));
            assert_eq!(width.decode(1), Err(TranslationEpochError::InvalidToken));
            assert_eq!(
                width.decode(1u64 << bits),
                Err(TranslationEpochError::InvalidToken)
            );
            assert_eq!(
                width.pack(1, width.max_epoch() + 1),
                Err(TranslationEpochError::Overflow)
            );
            if bits < 16 {
                assert_eq!(
                    width.pack(1u16 << bits, 1),
                    Err(TranslationEpochError::InvalidToken)
                );
            }
        }
    }

    #[test]
    fn packed_epoch_exhaustion_preserves_pins_and_allocation_state() {
        for bits in [8, 16] {
            let mut pool = pool();
            let width = ok(TranslationTagWidth::new(bits));
            let anchor = register(&mut pool, bits);
            pool.epoch = width.max_epoch() - 1;
            let pinned = ok(pool.acquire(&anchor, None));
            let original_tag = pinned.tag();
            ok(pool.advance_epoch());
            let second = ok(pool.acquire(&anchor, None));
            assert_eq!(second.binding(), pinned.binding());
            assert_eq!(second.tag().value() >> bits, width.max_epoch());
            assert_eq!(original_tag.value() >> bits, width.max_epoch() - 1);
            let bitmap = match pool.segments[0].as_ref() {
                Some(segment) => segment.free,
                None => panic!("missing segment"),
            };
            assert_eq!(pool.advance_epoch(), Err(TranslationEpochError::Overflow));
            assert_eq!(pool.epoch(), width.max_epoch());
            assert_eq!(pinned.tag(), original_tag);
            assert_eq!(
                pool.segments[0].as_ref().map(|segment| segment.free),
                Some(bitmap)
            );
            ok(pool.release(pinned));
            ok(pool.release(second));
            ok(pool.unregister_owner(anchor));
        }
    }

    #[test]
    fn register_is_transactional_and_width_is_immutable() {
        let mut pool = pool();
        assert!(ok(pool.register_owner(8, &mut None)).is_none());
        assert_eq!(
            (
                pool.width,
                pool.serial,
                pool.live_owners(),
                pool.segment_count
            ),
            (None, 0, 0, 0)
        );
        assert!(matches!(
            pool.register_owner(0, &mut None),
            Err(TranslationEpochError::InvalidWidth)
        ));
        assert!(matches!(
            pool.register_owner(17, &mut None),
            Err(TranslationEpochError::InvalidWidth)
        ));
        let owner = register(&mut pool, 8);
        assert_eq!(owner.serial(), 1);
        assert!(matches!(
            pool.register_owner(16, &mut None),
            Err(TranslationEpochError::WidthChanged)
        ));
        assert_eq!(pool.live_owners(), 1);
        ok(pool.unregister_owner(owner));
        assert!(matches!(
            pool.register_owner(16, &mut None),
            Err(TranslationEpochError::WidthChanged)
        ));
    }

    #[test]
    fn software_owners_exceed_hardware_capacity_and_roll_over() {
        let mut pool = pool();
        let owners: alloc::vec::Vec<_> = (0..300).map(|_| register(&mut pool, 8)).collect();
        assert_eq!(pool.segment_count, 1);
        for (index, owner) in owners.iter().enumerate() {
            let lease = ok(pool.acquire(owner, None));
            assert_eq!(lease.epoch(), if index < 255 { 1 } else { 2 });
            assert_ne!(lease.binding().id(), 0);
            assert_eq!(lease.binding().owner(), owner.serial());
            ok(pool.release(lease));
        }
        for owner in owners {
            ok(pool.unregister_owner(owner));
        }
        assert_eq!(pool.live_owners(), 0);
    }

    #[test]
    fn pinned_tag_survives_multiple_epochs_and_old_lease_releases() {
        let mut pool = pool();
        let pinned_owner = register(&mut pool, 2);
        let pinned = ok(pool.acquire(&pinned_owner, None));
        let binding = pinned.binding();
        let old_epoch = pinned.epoch();
        for _ in 0..20 {
            let owner = register(&mut pool, 2);
            let lease = ok(pool.acquire(&owner, None));
            assert_ne!(lease.binding().id(), binding.id());
            ok(pool.release(lease));
            ok(pool.unregister_owner(owner));
        }
        assert!(pool.epoch() > old_epoch + 2);
        let second = ok(pool.acquire(&pinned_owner, Some(binding)));
        assert_eq!(second.binding(), binding);
        assert_eq!(second.epoch(), pool.epoch());
        let pinned_owner = match pool.unregister_owner(pinned_owner) {
            Err((TranslationEpochError::Busy, owner)) => owner,
            other => panic!("active owner retirement accepted: {other:?}"),
        };
        ok(pool.release(pinned));
        ok(pool.release(second));
        ok(pool.unregister_owner(pinned_owner));
    }

    #[test]
    fn all_pinned_returns_busy_without_epoch_spin_or_state_change() {
        let mut pool = pool();
        let owners: alloc::vec::Vec<_> = (0..4).map(|_| register(&mut pool, 2)).collect();
        let leases: alloc::vec::Vec<_> = owners[..3]
            .iter()
            .map(|owner| ok(pool.acquire(owner, None)))
            .collect();
        let epoch = pool.epoch();
        for _ in 0..3 {
            assert!(matches!(
                pool.acquire(&owners[3], None),
                Err(TranslationEpochError::Busy)
            ));
            assert_eq!(pool.epoch(), epoch);
        }
        for lease in leases {
            ok(pool.release(lease));
        }
        let lease = ok(pool.acquire(&owners[3], None));
        assert_eq!(lease.epoch(), epoch + 1);
        ok(pool.release(lease));
        for owner in owners {
            ok(pool.unregister_owner(owner));
        }
    }

    #[test]
    fn stale_hint_never_steals_new_owner_or_duplicates_old_owner() {
        let mut pool = pool();
        let first = register(&mut pool, 1);
        let lease = ok(pool.acquire(&first, None));
        let stale = lease.binding();
        ok(pool.release(lease));
        ok(pool.unregister_owner(first));
        let second = register(&mut pool, 1);
        let lease = ok(pool.acquire(&second, Some(stale)));
        assert_ne!(lease.binding().owner(), stale.owner());
        assert_eq!(lease.epoch(), 2);
        let another = ok(pool.acquire(&second, Some(stale)));
        assert_eq!(lease.binding(), another.binding());
        ok(pool.release(lease));
        ok(pool.release(another));
        ok(pool.unregister_owner(second));
    }

    #[test]
    fn unregistered_cached_tag_is_not_reused_in_same_epoch() {
        let mut pool = pool();
        let first = register(&mut pool, 2);
        let lease = ok(pool.acquire(&first, None));
        let old = lease.binding();
        let epoch = lease.epoch();
        ok(pool.release(lease));
        ok(pool.unregister_owner(first));
        let second = register(&mut pool, 2);
        let lease = ok(pool.acquire(&second, None));
        assert_eq!(lease.epoch(), epoch);
        assert_ne!(lease.binding().id(), old.id());
        ok(pool.release(lease));
        ok(pool.unregister_owner(second));
    }

    #[test]
    fn storage_shrink_advances_epoch_and_respects_pins() {
        let mut pool = pool();
        let mut owners: alloc::vec::Vec<_> = (0..256).map(|_| register(&mut pool, 16)).collect();
        assert_eq!(pool.segment_count, 2);
        // Fill the first segment with cached tags and pin the first high tag.
        for owner in &owners[..255] {
            let lease = ok(pool.acquire(owner, None));
            ok(pool.release(lease));
        }
        let pinned_owner = match owners.pop() {
            Some(owner) => owner,
            None => panic!("owner missing"),
        };
        let lease = ok(pool.acquire(&pinned_owner, None));
        assert_eq!(lease.binding().id(), 256);
        for owner in owners {
            ok(pool.unregister_owner(owner));
        }
        assert!(pool.take_unused_segment().is_none());
        let old_epoch = pool.epoch();
        ok(pool.release(lease));
        let detached = pool.take_unused_segment();
        assert!(detached.is_some());
        assert_eq!(pool.segment_count, 1);
        assert_eq!(pool.epoch(), old_epoch + 1);
        drop(detached);
        let lease = ok(pool.acquire(&pinned_owner, None));
        assert!(lease.binding().id() < 256);
        ok(pool.release(lease));
        ok(pool.unregister_owner(pinned_owner));
        assert!(pool.take_unused_segment().is_some());
        assert_eq!(pool.segment_count, 0);
        let next = register(&mut pool, 16);
        let lease = ok(pool.acquire(&next, None));
        assert!(lease.epoch() > old_epoch + 1);
        ok(pool.release(lease));
        ok(pool.unregister_owner(next));
    }

    #[test]
    fn sixteen_bit_width_retains_last_hardware_tag_and_bounds_capacity() {
        let mut pool = pool();
        let mut owners: alloc::vec::Vec<_> = (0..65536).map(|_| register(&mut pool, 16)).collect();
        assert_eq!(pool.segment_count, 256);
        assert_eq!(pool.capacity(), 65536);
        for (index, owner) in owners[..65535].iter().enumerate() {
            let lease = ok(pool.acquire(owner, None));
            assert_eq!(lease.binding().id() as usize, index + 1);
            assert_eq!(lease.epoch(), 1);
            ok(pool.release(lease));
        }
        let owner = match owners.pop() {
            Some(owner) => owner,
            None => panic!("owner missing"),
        };
        let lease = ok(pool.acquire(&owner, None));
        assert_eq!(lease.binding().id(), 1);
        assert_eq!(lease.epoch(), 2);
        ok(pool.release(lease));
        ok(pool.unregister_owner(owner));
        for owner in owners {
            ok(pool.unregister_owner(owner));
        }
        assert_eq!(pool.live_owners(), 0);
    }

    #[test]
    fn overflow_preserves_owners_pins_storage_and_epoch() {
        let mut pool = pool();
        let first = register(&mut pool, 1);
        pool.serial = u64::MAX;
        assert!(matches!(
            pool.register_owner(1, &mut None),
            Err(TranslationEpochError::Overflow)
        ));
        assert_eq!(pool.live_owners(), 1);
        pool.serial = first.serial();
        let lease = ok(pool.acquire(&first, None));
        let binding = lease.binding();
        if let Some(slot) = pool.slot_mut(binding.id() as usize) {
            slot.pins = usize::MAX;
        }
        assert!(matches!(
            pool.acquire(&first, Some(binding)),
            Err(TranslationEpochError::Overflow)
        ));
        if let Some(slot) = pool.slot_mut(binding.id() as usize) {
            slot.pins = 1;
        }
        ok(pool.release(lease));
        ok(pool.unregister_owner(first));
        let last_epoch = ok(TranslationTagWidth::new(1)).max_epoch();
        pool.epoch = last_epoch;
        assert!(pool.take_unused_segment().is_none());
        assert_eq!((pool.epoch(), pool.segment_count), (last_epoch, 1));
        let second = register(&mut pool, 1);
        assert!(matches!(
            pool.acquire(&second, None),
            Err(TranslationEpochError::Overflow)
        ));
        assert_eq!(pool.epoch(), last_epoch);
        ok(pool.unregister_owner(second));
    }
}
