// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Thread representation and scheduling policy.

use hyper::sync::atomic::{AtomicBool, Ordering};

mod external_execution;
pub(crate) mod policy;
pub(crate) mod preempt;
mod reschedule;
pub mod scheduler;
pub(crate) mod scheduling_authority;
mod sleep;
#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
mod test_progress;
pub mod thread;
mod thread_object;
mod timeout;
mod wait;
mod wait_context;
pub(crate) use wait_context::{ThreadWaitContext, WaitSource, WaitSourceRegistration};

pub(crate) use thread_object::{
    ThreadObjectObservation, ThreadObjectRegistryPhase, ThreadObjectScanCursor,
    ThreadObjectSnapshot, ThreadObjectSnapshotPage, ThreadRole,
};

pub(crate) const fn system_thread_object_allocation_size() -> Option<usize> {
    thread_object::ThreadObject::system_allocation_size()
}

pub use sleep::{SleepError, sleep_ms, sleep_ns, sleep_s, sleep_until, sleep_us};
#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
pub(crate) use test_progress::{
    DEFAULT_TIMEOUT_NS as TEST_PROGRESS_TIMEOUT_NS, wait_until as wait_for_test_progress,
};
pub use timeout::TimedWaitError;
pub(crate) use timeout::{ArmedTimeout, PreparedTimeout};
pub(crate) use wait::{WaitMobility, WaitTicket};
pub use wait::{WaitOutcome, WaitQueue};

static READY: AtomicBool = AtomicBool::new(false);

/// Creates the bootstrap scheduling context and initial run queue.
pub(crate) fn initialize() -> Result<(), scheduler::Error> {
    let capabilities = scheduler::initialize()?;
    READY.store(true, Ordering::Release);
    crate::pr_info!(
        "HypeR: scheduler active on bootstrap thread {}",
        capabilities.bootstrap_thread.get()
    );
    Ok(())
}

pub(crate) fn is_ready() -> bool {
    READY.load(Ordering::Acquire)
}
