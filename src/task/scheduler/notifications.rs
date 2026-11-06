// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Bounded wait arbitration with coalesced ready publication.

use super::{
    CpuIndex, CpuMask, Error, RescheduleSource, WaitTicket, balance, read_scheduler,
    request_reschedule, scheduler_invariant,
};

pub(crate) const NOTIFICATION_BATCH_SIZE: usize = 16;

/// The source retains stable storage and serializes unregistration. The scheduler
/// arbitrates concurrent cancellation and timeout against this notification.
/// Only the winning generation may commit its observation, before becoming Ready.
/// Commit must be bounded, infallible, allocation-free, and must not re-enter
/// the scheduler; it runs under the owning CPU's scheduler lock.
pub(crate) trait WaitNotification {
    fn ticket(&self) -> WaitTicket;
    fn commit(&mut self);
}

/// Ready queues have already committed; publish their prompts after source unlock.
#[must_use = "publish the committed ready notifications"]
pub(crate) struct ReadyNotifications {
    cpus: CpuMask,
    #[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
    pub(crate) cpu_locks: u64,
    #[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
    pub(crate) made_ready: u64,
}

impl ReadyNotifications {
    pub(crate) const fn new() -> Self {
        Self {
            cpus: CpuMask::EMPTY,
            #[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
            cpu_locks: 0,
            #[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
            made_ready: 0,
        }
    }

    pub(super) fn record(&mut self, outcome: super::state::ReadyOutcome) {
        if outcome.changed {
            self.cpus = self.cpus.with_cpu(outcome.target_cpu);
        }
        #[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
        {
            self.made_ready += 1;
        }
    }

    pub(crate) fn publish(self) {
        if self.cpus.is_empty() {
            return;
        }
        balance::request();
        for index in 0..hyper::cpu::MAX_CPUS {
            let Some(cpu) = CpuIndex::new(index) else {
                scheduler_invariant("batch CPU identity", Error::InvalidCpuIndex);
            };
            if self.cpus.contains(cpu)
                && let Err(error) = request_reschedule(cpu, RescheduleSource::Wakeup)
            {
                scheduler_invariant("batch ready publication", error);
            }
        }
    }
}

/// One reader lane pins residence while each owner CPU is locked once per batch.
/// No source storage or callback escapes the caller's source lock.
pub(crate) fn notify_registered_batch<N: WaitNotification>(
    entries: &mut [Option<N>; NOTIFICATION_BATCH_SIZE],
    ready: &mut ReadyNotifications,
) -> Result<(), Error> {
    read_scheduler(|scheduler| scheduler.resolve_notifications_shared(entries, ready))
}
