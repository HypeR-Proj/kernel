// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! One allocation-free scheduled request, with generation-qualified completion.

use hyper::sync::InterruptSpinLock;

use {crate::mm::reclaim::ReclaimGuard, crate::mm::reclaim::Target};
use {crate::sync::Error, crate::sync::Mutex};
use {
    crate::task::WaitMobility, crate::task::WaitOutcome, crate::task::WaitQueue,
    crate::task::scheduler,
};

use super::request_state::{Request, RequestState};

static SERIALIZER: Mutex<()> = Mutex::new(());
static STATE: InterruptSpinLock<State, crate::hal::irq::LocalMask> =
    InterruptSpinLock::new(State {
        requests: RequestState::new(),
        waiters: WaitQueue::new(),
    });

struct State {
    requests: RequestState<Target>,
    waiters: WaitQueue,
}

pub(super) fn reclaim(target: Target) -> Option<ReclaimGuard> {
    scheduler::ensure_sleepable().ok()?;
    if !super::READY.load(core::sync::atomic::Ordering::Acquire)
        || matches!(target, Target::PhysicalOrder(order) if order > crate::mm::allocator::MAX_ORDER)
    {
        return None;
    }
    let cache = crate::fs::vfs::file_cache()?;
    let pause = cache.pause_admission();
    let serializer = SERIALIZER.lock().ok()?;
    let request = STATE.with(|state| state.requests.begin(target))?;
    let pending = PendingRequest(request.generation);
    super::request();
    let completed = wait(request.generation).is_ok();
    drop(pending);
    // Never carry request serialization into the caller's preparation retry:
    // that preparation can acquire locks held by another requesting thread.
    drop(serializer);
    completed.then_some(pause)
}

pub(super) fn pending() -> Option<Request<Target>> {
    STATE.with(|state| state.requests.pending())
}

pub(super) fn complete(generation: u64) {
    STATE.with(|state| {
        if state.requests.finish(generation)
            && let Err(error) = scheduler::wake_all(&state.waiters)
        {
            crate::diagnostics::crash::fatal(format_args!(
                "HypeR: scheduled reclaim wake failed: {error:?}"
            ));
        }
    });
}

fn wait(generation: u64) -> Result<(), Error> {
    loop {
        // SAFETY: Predicate testing and queue insertion share STATE. The
        // retained local mask is consumed by park or dropped on every exit.
        let (park, interrupt_mask) = unsafe {
            STATE.with_mask_retained(|state| {
                if state.requests.completed(generation) {
                    Ok(None)
                } else {
                    let registration = scheduler::begin_wait(WaitMobility::Migratable)?;
                    scheduler::prepare_registered_park_locked(&state.waiters, registration)
                        .map(Some)
                        .map_err(Error::from)
                }
            })
        };
        let Some(park) = park? else {
            drop(interrupt_mask);
            return Ok(());
        };
        let outcome = park.retain_mask(interrupt_mask).complete();
        if outcome != WaitOutcome::Notified {
            return Err(Error::WaitInterrupted(outcome));
        }
    }
}

struct PendingRequest(u64);

impl Drop for PendingRequest {
    fn drop(&mut self) {
        // Cancellation cannot retire a newer use of the slot. The worker can
        // finish a detached batch, but its delayed completion becomes a no-op.
        let cancelled = STATE.with(|state| state.requests.finish(self.0));
        if cancelled {
            super::request();
        }
    }
}
