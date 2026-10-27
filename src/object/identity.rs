// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Reusable diagnostic identity slots, owned until final object destruction.

use alloc::boxed::Box;
use core::num::{NonZeroU32, NonZeroU64};

use hyper::mm::try_box;
use hyper::sync::InterruptSpinLock;

use super::ObjectCreationError;

const _: () = assert!(hyper::abi::native::HYPER_NATIVE_KOID_SLOT_MASK == u32::MAX as u64);

#[cfg(not(test))]
type IdentityLock<T> = InterruptSpinLock<T, crate::hal::irq::LocalMask>;

#[cfg(test)]
struct TestInterruptMask;

#[cfg(test)]
impl hyper::hal::interrupt::InterruptMask for TestInterruptMask {
    type State = ();

    fn save_and_disable() -> Self::State {}

    fn restore(_state: Self::State) {}
}

#[cfg(test)]
type IdentityLock<T> = InterruptSpinLock<T, TestInterruptMask>;

/// Opaque diagnostic identity; neither a slot nor a KOID confers authority.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct Koid(NonZeroU64);

impl Koid {
    fn new(slot: NonZeroU32, generation: u32) -> Self {
        Self(NonZeroU64::from(slot) | (u64::from(generation) << u32::BITS))
    }

    pub(crate) const fn get(self) -> u64 {
        self.0.get()
    }

    fn next_generation(self) -> Self {
        let next = self.get().wrapping_add(1u64 << u32::BITS);
        // Keep generation zero unpublished, including after wrap. The slot
        // is unchanged and cannot be reused until its reservation is dropped.
        let next = if next >> u32::BITS == 0 {
            next | (1u64 << u32::BITS)
        } else {
            next
        };
        match NonZeroU64::new(next) {
            Some(next) => Self(next),
            None => hyper::debug::invariant_failure("KOID slot invariant"),
        }
    }
}

struct Slot {
    koid: Koid,
    next: Option<Box<Slot>>,
}

struct Slots {
    next_slot: Option<NonZeroU32>,
    available: Option<Box<Slot>>,
}

impl Slots {
    fn take_recycled(&mut self) -> Option<Box<Slot>> {
        let mut slot = self.available.take()?;
        self.available = slot.next.take();
        Some(slot)
    }

    fn take_fresh(&mut self) -> Result<Koid, ObjectCreationError> {
        let index = self.next_slot.ok_or(ObjectCreationError::KoidExhausted)?;
        self.next_slot = index.get().checked_add(1).and_then(NonZeroU32::new);
        Ok(Koid::new(index, 1))
    }
}

/// The free list retains one small node per reusable slot at the live-object
/// high-water mark, rather than allocating metadata on every object release.
pub(crate) struct KoidAllocator {
    slots: IdentityLock<Slots>,
}

impl KoidAllocator {
    pub(crate) const fn new() -> Self {
        Self {
            slots: IdentityLock::new(Slots {
                next_slot: Some(NonZeroU32::MIN),
                available: None,
            }),
        }
    }

    pub(crate) fn reserve(&self) -> Result<KoidReservation<'_>, ObjectCreationError> {
        let slot = match self.slots.with(Slots::take_recycled) {
            Some(slot) => slot,
            None => {
                // Prepare storage before taking the IRQ-masked lock. No slot
                // is consumed if allocation fails. This placeholder is never
                // published as an object identity.
                let mut storage = try_box(Slot {
                    koid: Koid(NonZeroU64::MIN),
                    next: None,
                })?;
                let recycled = self.slots.with(|slots| {
                    if let Some(slot) = slots.take_recycled() {
                        return Ok(Some(slot));
                    }
                    storage.koid = slots.take_fresh()?;
                    Ok::<_, ObjectCreationError>(None)
                })?;
                // Concurrent retirement may have supplied a slot while we
                // allocated. Dispose of surplus storage outside the lock.
                match recycled {
                    Some(slot) => slot,
                    None => storage,
                }
            }
        };
        Ok(KoidReservation {
            koid: slot.koid,
            slot: Some(slot),
            allocator: self,
        })
    }

    #[cfg(test)]
    pub(crate) fn with_last_slot_for_test() -> Self {
        Self {
            slots: IdentityLock::new(Slots {
                next_slot: NonZeroU32::new(u32::MAX),
                available: None,
            }),
        }
    }
}

impl Drop for KoidAllocator {
    fn drop(&mut self) {
        // The global allocator is never dropped; local test allocators must
        // not recursively destroy an arbitrarily long free list.
        while let Some(slot) = self.slots.with(Slots::take_recycled) {
            drop(slot);
        }
    }
}

/// Linear slot ownership. The object's final field retains it through payload
/// destruction; construction failure returns it through the same path.
pub(crate) struct KoidReservation<'allocator> {
    koid: Koid,
    slot: Option<Box<Slot>>,
    allocator: &'allocator KoidAllocator,
}

impl KoidReservation<'_> {
    pub(crate) const fn koid(&self) -> Koid {
        self.koid
    }

    pub(super) const fn allocation_size() -> usize {
        core::mem::size_of::<Slot>()
    }

    #[cfg(test)]
    pub(crate) fn exhaust_generation_for_test(&mut self) {
        if let Some(slot) = self.slot.as_mut() {
            self.koid = Koid(self.koid.0 | (u64::from(u32::MAX) << u32::BITS));
            slot.koid = self.koid;
        }
    }
}

impl Drop for KoidReservation<'_> {
    fn drop(&mut self) {
        if let Some(mut slot) = self.slot.take() {
            slot.koid = slot.koid.next_generation();
            self.allocator.slots.with(|slots| {
                slot.next = slots.available.take();
                slots.available = Some(slot);
            });
        }
    }
}

static IDENTITIES: KoidAllocator = KoidAllocator::new();

pub(super) fn reserve() -> Result<KoidReservation<'static>, ObjectCreationError> {
    IDENTITIES.reserve()
}
