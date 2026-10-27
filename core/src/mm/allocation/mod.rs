// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Fallible ownership primitives built on the installed global allocator.

use alloc::boxed::Box;
use core::alloc::Layout;
use core::marker::PhantomData;
use core::mem::{ManuallyDrop, MaybeUninit};
use core::ops::{Deref, DerefMut};
use core::ptr::NonNull;
use core::sync::atomic::{AtomicUsize, Ordering};

mod refcount;

use refcount::StrongRelease;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AllocationError;

#[repr(C)]
struct SharedInner<T> {
    strong: AtomicUsize,
    weak: AtomicUsize,
    value: ManuallyDrop<T>,
}

/// A fallibly allocated, thread-safe shared owner.
///
/// This deliberately small primitive exists because the stable allocator API
/// does not provide `Arc::try_new`. Once either reference counter saturates,
/// the allocation is permanently retained; leaking is preferable to wrapping
/// a counter and freeing a live value.
pub struct FallibleArc<T> {
    inner: NonNull<SharedInner<T>>,
    ownership: PhantomData<SharedInner<T>>,
}

/// Non-owning observer of one [`FallibleArc`] allocation.
///
/// A weak owner keeps only the allocation header alive. [`Self::upgrade`]
/// succeeds exactly while a strong owner still protects the initialized value.
pub struct WeakFallibleArc<T> {
    inner: NonNull<SharedInner<T>>,
    ownership: PhantomData<SharedInner<T>>,
}

/// Linear responsibility for destroying a value after its final shared owner
/// has been released.
///
/// The strong count is already zero while this value exists, so weak upgrades
/// permanently fail. Dropping this owner destroys `T` and releases the
/// allocation's implicit weak reference. This is a low-level deferred-drop
/// mechanism, not an authority or a shareable object reference.
#[must_use = "dropping this owner completes the deferred destruction"]
pub struct DeferredArcDrop<T> {
    inner: NonNull<SharedInner<T>>,
    ownership: PhantomData<SharedInner<T>>,
}

/// Unique owner which retains a `FallibleArc` allocation without refcounting.
///
/// This is the linear teardown form for large hardware owners: conversion is
/// allocation-free, the value remains pinned, and sharing can be restored
/// without moving the payload.
pub struct UniqueFallibleArc<T> {
    inner: NonNull<SharedInner<T>>,
    ownership: PhantomData<SharedInner<T>>,
}

// SAFETY: UniqueFallibleArc has exclusive ownership, so moving the owner
// across CPUs is sound exactly when moving T is sound.
unsafe impl<T: Send> Send for UniqueFallibleArc<T> {}
// SAFETY: Shared references to the uniquely owned value may cross CPUs exactly
// when shared references to T may cross CPUs.
unsafe impl<T: Sync> Sync for UniqueFallibleArc<T> {}

// SAFETY: FallibleArc grants shared access to T from every clone. Moving or
// sharing that access across CPUs is sound exactly when T is Send + Sync.
unsafe impl<T: Send + Sync> Send for FallibleArc<T> {}
// SAFETY: See the Send implementation. Reference-count changes are atomic and
// the contained value is exposed only through shared references.
unsafe impl<T: Send + Sync> Sync for FallibleArc<T> {}

// SAFETY: WeakFallibleArc never exposes T without atomically acquiring a
// strong owner. Moving or sharing that capability follows the same bounds as
// FallibleArc.
unsafe impl<T: Send + Sync> Send for WeakFallibleArc<T> {}
// SAFETY: See the Send implementation. Weak-count mutation is atomic.
unsafe impl<T: Send + Sync> Sync for WeakFallibleArc<T> {}

// SAFETY: DeferredArcDrop is the sole owner of an initialized T after the
// strong count reached zero. Moving final-destruction responsibility across
// CPUs is sound exactly when moving T is sound.
unsafe impl<T: Send> Send for DeferredArcDrop<T> {}
// SAFETY: Shared access through DeferredArcDrop may cross CPUs exactly when
// shared access to T may cross CPUs. No API can restore a strong owner.
unsafe impl<T: Sync> Sync for DeferredArcDrop<T> {}

impl<T> FallibleArc<T> {
    /// Returns the allocator-requested size of one shared allocation.
    pub const fn allocation_size() -> usize {
        core::mem::size_of::<SharedInner<T>>()
    }

    /// Allocates one shared owner without invoking the allocation-error path.
    pub fn try_new(value: T) -> Result<Self, AllocationError> {
        Self::try_new_or_return(value).map_err(|(error, _)| error)
    }

    /// Allocates one shared owner while preserving `value` on failure.
    pub fn try_new_or_return(value: T) -> Result<Self, (AllocationError, T)> {
        // SAFETY: The installed global allocator satisfies its own deallocation contract.
        unsafe { Self::try_new_or_return_in(value, |layout| alloc::alloc::alloc(layout)) }
    }

    /// Allocates through a specialized pool of the installed global allocator.
    ///
    /// # Safety
    /// A non-null callback result must satisfy `layout` and be releasable by
    /// the installed global allocator with that exact layout. Ownership moves
    /// here; no other pointer owner may access or release the allocation.
    pub unsafe fn try_new_or_return_in(
        value: T,
        allocate: impl FnOnce(Layout) -> *mut u8,
    ) -> Result<Self, (AllocationError, T)> {
        // SAFETY: The callback inherits the global-deallocation contract above.
        let inner = unsafe {
            try_box_or_return_in(
                SharedInner {
                    strong: AtomicUsize::new(1),
                    // One implicit weak reference keeps the allocation alive while
                    // any strong owner exists.
                    weak: AtomicUsize::new(1),
                    value: ManuallyDrop::new(value),
                },
                allocate,
            )
        }
        .map_err(|(error, inner)| (error, ManuallyDrop::into_inner(inner.value)))?;
        // SAFETY: Box never contains a null pointer. FallibleArc assumes the
        // allocation and becomes responsible for reconstructing the Box after
        // the final non-saturated reference is released.
        let inner = unsafe { NonNull::new_unchecked(Box::into_raw(inner)) };
        Ok(Self {
            inner,
            ownership: PhantomData,
        })
    }

    /// Returns a non-owning reference-count snapshot for diagnostics.
    pub fn strong_count(&self) -> usize {
        refcount::strong_count(&self.inner().strong)
    }

    /// Observes whether these two distinct owners are the entire strong set.
    ///
    /// The acquire RMW observes the latest ownership transition rather than a
    /// diagnostic count. This neither reserves ownership nor establishes an
    /// exclusive borrow. Stable ownership proofs must separately close future
    /// cloning/weak upgrades and account for borrowed references; in particular
    /// the weak-count observations cannot exclude a concurrent upgrade/drop.
    pub fn is_only_owner_pair(&self, other: &Self) -> bool {
        if core::ptr::eq(self, other) || self.inner != other.inner {
            return false;
        }
        let inner = self.inner();
        inner.weak.load(Ordering::Acquire) == 1
            && inner
                .strong
                .compare_exchange(2, 2, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            && inner.weak.load(Ordering::Acquire) == 1
    }

    /// Creates a non-owning allocation reference.
    pub fn downgrade(&self) -> WeakFallibleArc<T> {
        refcount::retain_weak(&self.inner().weak);
        WeakFallibleArc {
            inner: self.inner,
            ownership: PhantomData,
        }
    }

    /// Releases this shared owner without destroying the final value inline.
    ///
    /// A non-final release returns `None`. The unique caller which changes the
    /// strong count from one to zero receives the deferred-drop owner. Weak
    /// upgrades fail from that transition onward, but `T` remains initialized
    /// until the returned owner is dropped.
    ///
    /// A saturated reference count retains the existing leak-on-overflow
    /// behavior and returns `None`.
    pub fn release_deferred(self) -> Option<DeferredArcDrop<T>> {
        let owner = ManuallyDrop::new(self);
        match refcount::release_strong(&owner.inner().strong) {
            StrongRelease::Final => Some(DeferredArcDrop {
                inner: owner.inner,
                ownership: PhantomData,
            }),
            StrongRelease::Shared | StrongRelease::Leaked => None,
        }
    }

    /// Extracts the value when this is the unique, non-saturated owner.
    ///
    /// Failure returns the original owner unchanged. This is the retirement
    /// boundary for resources which may be shared while active but require a
    /// linear owner for final hardware teardown.
    pub fn try_unwrap(self) -> Result<T, Self> {
        self.try_into_unique().map(UniqueFallibleArc::into_inner)
    }

    /// Converts the sole shared reference into a pinned linear owner.
    pub fn try_into_unique(self) -> Result<UniqueFallibleArc<T>, Self> {
        let inner = self.inner();
        // The refcount transition excludes both other strong owners and
        // external weak observers. A concurrent weak upgrade cancels the
        // transition without observing a temporary dead value.
        if !refcount::try_unique(&inner.strong, &inner.weak) {
            return Err(self);
        }
        let owner = ManuallyDrop::new(self);
        Ok(UniqueFallibleArc {
            inner: owner.inner,
            ownership: PhantomData,
        })
    }

    fn inner(&self) -> &SharedInner<T> {
        // SAFETY: Every FallibleArc clone retains one reference. The allocation
        // cannot be freed while `self` is alive, and the pointee never moves.
        unsafe { self.inner.as_ref() }
    }
}

impl<T> WeakFallibleArc<T> {
    /// Reports whether a strong owner existed at the instant of observation.
    ///
    /// This is diagnostic only: the value may disappear immediately after
    /// this method returns. Use [`Self::upgrade`] when access to `T` is needed.
    pub fn is_alive(&self) -> bool {
        self.inner().strong.load(Ordering::Acquire) != 0
    }

    /// Acquires a strong owner while the value remains alive.
    pub fn upgrade(&self) -> Option<FallibleArc<T>> {
        if !refcount::try_upgrade(&self.inner().strong) {
            return None;
        }
        Some(FallibleArc {
            inner: self.inner,
            ownership: PhantomData,
        })
    }

    fn inner(&self) -> &SharedInner<T> {
        // SAFETY: Every WeakFallibleArc retains one weak reference, so the
        // allocation header cannot be freed while `self` is alive.
        unsafe { self.inner.as_ref() }
    }
}

impl<T> UniqueFallibleArc<T> {
    pub const fn allocation_size() -> usize {
        core::mem::size_of::<SharedInner<T>>()
    }

    /// Restores one shared reference without allocating or moving `T`.
    pub fn into_shared(self) -> FallibleArc<T> {
        let owner = ManuallyDrop::new(self);
        // No FallibleArc exists while the unique token is live. Release makes
        // all unique-owner mutation visible before future Arc clones.
        // SAFETY: The unique owner keeps the allocation live and excludes
        // concurrent access to the zero-valued reference counter.
        unsafe { owner.inner.as_ref() }
            .strong
            .store(1, Ordering::Release);
        FallibleArc {
            inner: owner.inner,
            ownership: PhantomData,
        }
    }

    /// Extracts the value and releases the retained shared allocation.
    pub fn into_inner(self) -> T {
        let owner = ManuallyDrop::new(self);
        // SAFETY: A UniqueFallibleArc is the only owner and keeps the count at
        // zero, so exactly this operation may reconstruct the allocation.
        // SAFETY: The unique owner excludes weak references and keeps the
        // allocation live with both counters in their unique state.
        let mut inner = unsafe { Box::from_raw(owner.inner.as_ptr()) };
        // SAFETY: UniqueFallibleArc owns the one initialized T.
        unsafe { ManuallyDrop::take(&mut inner.value) }
    }
}

impl<T> Deref for DeferredArcDrop<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        // SAFETY: the final-release transition leaves the value initialized
        // and transfers its sole destruction responsibility into this owner.
        // A zero strong count prevents every weak reference from upgrading.
        unsafe { &self.inner.as_ref().value }
    }
}

impl<T> Drop for DeferredArcDrop<T> {
    fn drop(&mut self) {
        destroy_value_and_release_implicit_weak(self.inner);
    }
}

impl<T> UniqueFallibleArc<MaybeUninit<T>> {
    /// Allocates a pinned linear slot before fallible hardware publication.
    pub fn try_new_uninit() -> Result<Self, AllocationError> {
        FallibleArc::try_new(MaybeUninit::uninit()).map(|owner| {
            // The newly created Arc is uniquely owned by construction.
            match owner.try_into_unique() {
                Ok(unique) => unique,
                Err(_) => unreachable_unique_creation(),
            }
        })
    }

    /// Initializes the retained slot without moving the resulting owner.
    pub fn write(self, value: T) -> UniqueFallibleArc<T> {
        let owner = ManuallyDrop::new(self);
        // SAFETY: MaybeUninit<T> has the same layout as T. This unique owner
        // grants exclusive access to the value field, which is written once.
        unsafe {
            core::ptr::addr_of_mut!((*owner.inner.as_ptr()).value)
                .write(ManuallyDrop::new(MaybeUninit::new(value)));
        }
        UniqueFallibleArc {
            inner: owner.inner.cast(),
            ownership: PhantomData,
        }
    }
}

impl<T> Deref for UniqueFallibleArc<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        // SAFETY: The unique allocation stays live and initialized.
        unsafe { &self.inner.as_ref().value }
    }
}

impl<T> DerefMut for UniqueFallibleArc<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        // SAFETY: The linear owner excludes every FallibleArc and other unique
        // owner, so it grants exclusive access to the pinned value.
        unsafe { &mut self.inner.as_mut().value }
    }
}

impl<T> Drop for UniqueFallibleArc<T> {
    fn drop(&mut self) {
        // SAFETY: Unique ownership, a zero strong count, and absence of
        // external weak owners prove this is the only value destructor and
        // allocation owner.
        unsafe {
            ManuallyDrop::drop(&mut self.inner.as_mut().value);
            drop(Box::from_raw(self.inner.as_ptr()));
        }
    }
}

#[cold]
fn unreachable_unique_creation() -> ! {
    // A newly allocated FallibleArc has exactly one reference. Keep this
    // dependency-free primitive fail-stop if its own constructor is violated.
    crate::debug::invariant_failure("new FallibleArc is not uniquely owned")
}

impl<T> Clone for FallibleArc<T> {
    fn clone(&self) -> Self {
        refcount::retain_strong(&self.inner().strong);
        Self {
            inner: self.inner,
            ownership: PhantomData,
        }
    }
}

impl<T> Deref for FallibleArc<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.inner().value
    }
}

impl<T> Drop for FallibleArc<T> {
    fn drop(&mut self) {
        if refcount::release_strong(&self.inner().strong) == StrongRelease::Final {
            destroy_value_and_release_implicit_weak(self.inner);
        }
    }
}

impl<T> Clone for WeakFallibleArc<T> {
    fn clone(&self) -> Self {
        refcount::retain_weak(&self.inner().weak);
        Self {
            inner: self.inner,
            ownership: PhantomData,
        }
    }
}

impl<T> Drop for WeakFallibleArc<T> {
    fn drop(&mut self) {
        release_weak(self.inner);
    }
}

fn destroy_value_and_release_implicit_weak<T>(inner: NonNull<SharedInner<T>>) {
    // SAFETY: only the caller which completed the one-to-zero strong
    // transition can reach this helper, either directly or through the unique
    // DeferredArcDrop it created. Weak upgrades cannot succeed at zero.
    unsafe { ManuallyDrop::drop(&mut (*inner.as_ptr()).value) };
    release_weak(inner);
}

fn release_weak<T>(inner: NonNull<SharedInner<T>>) {
    // SAFETY: The caller owns one weak reference, so the allocation header is
    // live for this decrement.
    let weak = unsafe { &inner.as_ref().weak };
    if !refcount::release_weak(weak) {
        return;
    }
    // SAFETY: This thread released the final weak reference after the strong
    // count reached zero and the ManuallyDrop value was already destroyed.
    unsafe { drop(Box::from_raw(inner.as_ptr())) };
}

/// Allocates and initializes one owned value without invoking the infallible
/// allocation-error path.
pub fn try_box<T>(value: T) -> Result<Box<T>, AllocationError> {
    try_box_or_return(value).map_err(|(error, _)| error)
}

/// Fallibly allocates one uninitialized `Box` without materializing `T` on the
/// caller's stack.
///
/// Large fixed-capacity objects can initialize this storage in place before
/// converting it to `Box<T>`.
pub fn try_box_uninit<T>() -> Result<Box<MaybeUninit<T>>, AllocationError> {
    let layout = Layout::new::<MaybeUninit<T>>();
    if layout.size() == 0 {
        return Ok(Box::new(MaybeUninit::uninit()));
    }
    // SAFETY: a successful allocation has the exact layout required by
    // `MaybeUninit<T>` and does not claim that a `T` is initialized.
    let Some(pointer) = NonNull::new(unsafe { alloc::alloc::alloc(layout) } as *mut MaybeUninit<T>)
    else {
        return Err(AllocationError);
    };
    // SAFETY: `pointer` is aligned, uniquely owned, and was allocated with the
    // layout used by `Box<MaybeUninit<T>>`.
    Ok(unsafe { Box::from_raw(pointer.as_ptr()) })
}

/// Allocates one owned value while preserving it on allocation failure.
pub fn try_box_or_return<T>(value: T) -> Result<Box<T>, (AllocationError, T)> {
    // SAFETY: Matching allocation and destruction both use the global allocator.
    unsafe { try_box_or_return_in(value, |layout| alloc::alloc::alloc(layout)) }
}

unsafe fn try_box_or_return_in<T>(
    value: T,
    allocate: impl FnOnce(Layout) -> *mut u8,
) -> Result<Box<T>, (AllocationError, T)> {
    let layout = Layout::new::<T>();
    if layout.size() == 0 {
        // Box does not allocate storage for a zero-sized type. Using the safe
        // constructor is important here because it moves `value` into the Box;
        // reconstructing a Box from a dangling pointer would instead leave a
        // zero-sized value with Drop to be destroyed twice.
        return Ok(Box::new(value));
    }
    // SAFETY: A successful allocation has the exact layout required by T.
    let Some(pointer) = NonNull::new(allocate(layout) as *mut T) else {
        return Err((AllocationError, value));
    };
    // SAFETY: pointer is aligned, writable, and uniquely owned for one T.
    unsafe {
        pointer.as_ptr().write(value);
        Ok(Box::from_raw(pointer.as_ptr()))
    }
}
