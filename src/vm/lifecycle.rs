// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Deferred ownership of installed VM lifecycle authority.

use hyper::mm::FallibleArc;
use hyper::sync::InterruptSpinLock;

use super::installed::InstalledMachine;
use super::registry::{QuiescePoll, QuiescentControl, QuiescingVm, VmControl};
use super::retirement_observation::RetirementObservation;

type QueueLock = InterruptSpinLock<RetirementQueue, crate::hal::irq::LocalMask>;

static RETIREMENT_QUEUE: QueueLock = InterruptSpinLock::new(RetirementQueue::new());

struct RetirementWork {
    owner: FallibleArc<InstalledMachine>,
    phase: RetirementPhase,
    observation: RetirementObservation,
}

enum RetirementPhase {
    Begin(VmControl),
    Quiescing(QuiescingVm),
    Retire(QuiescentControl),
}

struct RetirementQueue {
    // Each installed registry slot owns at most one VmControl, and enqueue
    // consumes it. Matching the registry capacity therefore guarantees that
    // every live VM can publish exactly one allocation-free retirement.
    entries: [Option<RetirementWork>; super::registry::MAX_VIRTUAL_MACHINES],
    head: usize,
    count: usize,
}

impl RetirementQueue {
    const fn new() -> Self {
        Self {
            entries: [const { None }; super::registry::MAX_VIRTUAL_MACHINES],
            head: 0,
            count: 0,
        }
    }

    fn push(&mut self, work: RetirementWork) {
        if self.count == self.entries.len() {
            hyper::debug::invariant_failure("vm::lifecycle::push invariant");
        }
        let index = (self.head + self.count) % self.entries.len();
        if self.entries[index].replace(work).is_some() {
            hyper::debug::invariant_failure("vm::lifecycle::push invariant");
        }
        self.count += 1;
    }

    fn pop(&mut self) -> Option<RetirementWork> {
        if self.count == 0 {
            return None;
        }
        let work = self.entries[self.head].take();
        self.head = (self.head + 1) % self.entries.len();
        self.count -= 1;
        match work {
            Some(work) => Some(work),
            None => hyper::debug::invariant_failure("vm::lifecycle::pop invariant"),
        }
    }

    const fn has_work(&self) -> bool {
        self.count != 0
    }
}

pub(super) fn enqueue(owner: FallibleArc<InstalledMachine>, control: VmControl) {
    RETIREMENT_QUEUE.with(|queue| {
        queue.push(RetirementWork {
            owner,
            phase: RetirementPhase::Begin(control),
            observation: RetirementObservation::new(),
        });
    });
    crate::reaper::request();
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ReapBatch {
    pub(crate) made_progress: bool,
    pub(crate) has_work: bool,
    pub(crate) needs_retry: bool,
}

impl ReapBatch {
    const EMPTY: Self = Self {
        made_progress: false,
        has_work: false,
        needs_retry: false,
    };

    pub(crate) const fn continue_immediately(self) -> bool {
        self.made_progress && self.has_work
    }
}

enum ReapOutcome {
    Progress,
    Deferred,
}

/// Visits every item which was queued at the start of this bounded pass once.
///
/// Deferred work is appended at the tail, so snapshotting the queue length
/// prevents one non-quiescent VM from being polled repeatedly while unrelated
/// VMs wait behind it. A caller may perform a second pass after scheduler
/// retirement, then delay only when that complete pass made no progress.
pub(crate) fn reap_batch(_access: &mut crate::reaper::ReaperAccess) -> ReapBatch {
    let visits = RETIREMENT_QUEUE.with(|queue| queue.count);
    if visits == 0 {
        return ReapBatch::EMPTY;
    }
    let mut batch = ReapBatch::EMPTY;
    for _ in 0..visits {
        match reap_one() {
            Some(ReapOutcome::Progress) => batch.made_progress = true,
            Some(ReapOutcome::Deferred) => batch.needs_retry = true,
            None => break,
        }
    }
    batch.has_work = RETIREMENT_QUEUE.with(|queue| queue.has_work());
    batch
}

/// Advances one VM through stop, vCPU quiescence, and stage-2 retirement.
fn reap_one() -> Option<ReapOutcome> {
    let mut work = RETIREMENT_QUEUE.with(RetirementQueue::pop)?;
    work.phase = match work.phase {
        RetirementPhase::Begin(control) => match control.begin() {
            Ok(quiescing) => RetirementPhase::Quiescing(quiescing),
            Err(failure) => crate::diagnostics::crash::fatal(format_args!(
                "HypeR: VM retirement could not begin: {:?}",
                failure.error()
            )),
        },
        phase => phase,
    };
    work.phase = match work.phase {
        RetirementPhase::Quiescing(quiescing) => match quiescing.poll() {
            QuiescePoll::Pending(quiescing) => {
                work.phase = RetirementPhase::Quiescing(quiescing);
                RETIREMENT_QUEUE.with(|queue| queue.push(work));
                return Some(ReapOutcome::Deferred);
            }
            QuiescePoll::Quiescent(control) => RetirementPhase::Retire(control),
        },
        phase => phase,
    };
    let RetirementPhase::Retire(control) = work.phase else {
        hyper::debug::invariant_failure("vm::lifecycle::reap_one invariant")
    };
    let id = control.id();
    match control.retire() {
        Ok(()) => {
            work.owner.publish_stopped();
            if work.observation.failures() != 0 {
                crate::pr_info!(
                    "HypeR: VM {id:?} retirement completed after {} failed attempts",
                    work.observation.failures()
                );
            }
            Some(ReapOutcome::Progress)
        }
        Err(failure) => {
            let error = failure.error();
            if work.observation.record(error) {
                crate::pr_warn!(
                    "HypeR: VM {id:?} retirement deferred: {error:?}; retaining resources and retrying"
                );
            }
            work.phase = RetirementPhase::Retire(failure.into_control());
            RETIREMENT_QUEUE.with(|queue| queue.push(work));
            Some(ReapOutcome::Deferred)
        }
    }
}
