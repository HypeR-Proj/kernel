// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Architecture-independent synchronization primitives.

pub mod atomic;
mod atomic_borrow;
mod deferred_work;
mod generation;
pub mod lock;
mod publication;

pub use atomic_borrow::{AtomicBorrowClaim, AtomicBorrowError, AtomicBorrowPtr};
pub use deferred_work::{DeferredWork, WorkDisposition};
pub use generation::GenerationTaggedState;
pub use lock::{InterruptMaskGuard, InterruptSpinLock, SpinLock};
pub use publication::{PublishError, PublishedOnce};
