// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Portable atomic primitives supplied by Rust's `core` library.
//!
//! LLVM lowers these operations to the target's supported instruction set. The
//! `AArch64` build uses outlined atomics: EL2 runtime feature discovery selects
//! LSE on capable CPUs while retaining LL/SC for the minimal Armv8-A target.
//! Only stable `core` atomic types are exported; Rust does not yet stabilize its
//! 128-bit integer atomic API.

// Keep standard-library documentation and its book links at their upstream URLs.
#[doc(no_inline)]
pub use core::sync::atomic::{AtomicBool, AtomicPtr, Ordering, compiler_fence, fence};

#[doc(no_inline)]
#[cfg(target_has_atomic = "8")]
pub use core::sync::atomic::{AtomicI8, AtomicU8};
#[doc(no_inline)]
#[cfg(target_has_atomic = "16")]
pub use core::sync::atomic::{AtomicI16, AtomicU16};
#[doc(no_inline)]
#[cfg(target_has_atomic = "32")]
pub use core::sync::atomic::{AtomicI32, AtomicU32};
#[doc(no_inline)]
#[cfg(target_has_atomic = "64")]
pub use core::sync::atomic::{AtomicI64, AtomicU64};
#[doc(no_inline)]
#[cfg(target_has_atomic = "ptr")]
pub use core::sync::atomic::{AtomicIsize, AtomicUsize};

/// A small acquire/release ownership flag for lock-free state transitions.
pub struct AtomicFlag {
    value: AtomicBool,
}

impl AtomicFlag {
    pub const fn new(acquired: bool) -> Self {
        Self {
            value: AtomicBool::new(acquired),
        }
    }

    pub fn try_acquire(&self) -> bool {
        self.value
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
    }

    pub fn release(&self) {
        self.value.store(false, Ordering::Release);
    }

    /// Returns a non-synchronizing snapshot of the ownership bit.
    ///
    /// This observation does not grant ownership and must not be used to read
    /// data protected by the flag. Keeping the ordering internal also avoids
    /// accepting `Release` or `AcqRel`, which are invalid for atomic loads.
    pub fn is_acquired(&self) -> bool {
        self.value.load(Ordering::Relaxed)
    }
}

impl Default for AtomicFlag {
    fn default() -> Self {
        Self::new(false)
    }
}
