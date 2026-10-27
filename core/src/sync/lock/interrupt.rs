// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Lock wrapper that composes local interrupt masking with a raw spin lock.

use core::marker::PhantomData;

use crate::hal::interrupt::InterruptMask;

use super::SpinLock;

/// A spin lock that disables local interrupts before acquiring its raw lock.
///
/// This prevents an interrupt handler on the same processing element from
/// attempting to recursively acquire a lock held by interrupted code. The
/// architecture policy remains outside generic synchronization code.
pub struct InterruptSpinLock<T, M: InterruptMask> {
    lock: SpinLock<T>,
    policy: PhantomData<fn() -> M>,
}

/// Ownership of one saved local-interrupt state.
///
/// The guard may outlive the lock acquisition that created it. A scheduler
/// transition of a migratable continuation must consume it with
/// [`Self::into_restore_state`] before the machine handoff. The guard may
/// remain on a suspended stack only while the scheduler guarantees that
/// continuation stays pinned to this CPU until the saved state is restored.
///
/// The guard is CPU-affine and therefore neither `Send` nor `Sync`. It must be
/// destroyed on the CPU that acquired it, in strict reverse acquisition order
/// within one active continuation and mask-state lineage unless its state is
/// transferred into the scheduler-owned machine context as documented below.
pub struct InterruptMaskGuard<M: InterruptMask> {
    state: M::State,
    policy: PhantomData<fn() -> M>,
    // Raw pointers implement neither Send nor Sync, making mask ownership
    // explicitly CPU-affine without depending on the policy's auto traits.
    cpu_affine: PhantomData<*mut ()>,
}

impl<M: InterruptMask> InterruptMaskGuard<M> {
    /// Captures and masks the calling CPU's local interrupt state.
    ///
    /// # Safety
    ///
    /// The returned guard must be dropped on this CPU in strict reverse order
    /// within its continuation's mask-state lineage. A scheduler must either
    /// keep a suspended continuation pinned to this CPU until restoration or
    /// consume the guard into the continuation's machine context before it can
    /// migrate.
    pub unsafe fn acquire() -> Self {
        Self {
            state: M::save_and_disable(),
            policy: PhantomData,
            cpu_affine: PhantomData,
        }
    }

    /// Transfers restoration ownership to a machine context.
    ///
    /// # Safety
    ///
    /// The caller must arrange for the returned state to be installed as the
    /// continuation's interrupt state before that continuation can execute
    /// again. It must not also restore the state on the calling CPU. The mask
    /// policy must permit its saved state to follow that continuation to any
    /// CPU on which the scheduler may resume it.
    pub unsafe fn into_restore_state(self) -> M::State {
        let state = self.state;
        core::mem::forget(self);
        state
    }
}

impl<M: InterruptMask> Drop for InterruptMaskGuard<M> {
    fn drop(&mut self) {
        M::restore(self.state);
    }
}

impl<T, M: InterruptMask> InterruptSpinLock<T, M> {
    pub const fn new(value: T) -> Self {
        Self {
            lock: SpinLock::new(value),
            policy: PhantomData,
        }
    }

    /// Extracts the value when no shared lock reference can remain.
    pub fn into_inner(self) -> T {
        self.lock.into_inner()
    }

    pub fn with<R>(&self, operation: impl FnOnce(&mut T) -> R) -> R {
        // SAFETY: This lexical guard cannot escape, and nested `with` calls
        // necessarily destroy their guards before this one.
        let restore = unsafe { InterruptMaskGuard::<M>::acquire() };
        let result = self.lock.with_relax(operation, M::wait_for_lock_owner);
        drop(restore);
        result
    }

    /// Releases the lock but transfers interrupt restoration to the caller.
    ///
    /// This is intended for lock-to-scheduler handoff. The returned guard must
    /// be dropped when no transition is needed or consumed into the outgoing
    /// machine context immediately before a context switch.
    ///
    /// # Safety
    ///
    /// The caller assumes ownership of the returned guard and must drop it on
    /// this CPU in strict reverse order within its mask-state lineage, or use
    /// [`InterruptMaskGuard::into_restore_state`] under that method's contract.
    pub unsafe fn with_mask_retained<R>(
        &self,
        operation: impl FnOnce(&mut T) -> R,
    ) -> (R, InterruptMaskGuard<M>) {
        // SAFETY: The caller accepts the retained guard's CPU and nesting
        // obligations stated by this method.
        let restore = unsafe { InterruptMaskGuard::<M>::acquire() };
        let result = self.lock.with_relax(operation, M::wait_for_lock_owner);
        (result, restore)
    }

    /// Runs `operation` only when the underlying lock is immediately free.
    pub fn try_with<R>(&self, operation: impl FnOnce(&mut T) -> R) -> Option<R> {
        // SAFETY: This lexical guard cannot escape, and nested `try_with` calls
        // necessarily destroy their guards before this one.
        let restore = unsafe { InterruptMaskGuard::<M>::acquire() };
        let result = self.lock.try_with(operation);
        drop(restore);
        result
    }
}
