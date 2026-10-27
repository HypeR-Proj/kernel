// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Forces one remote update to meet the scheduler's masked detach retry.

use hyper::cpu::CpuIndex;
use hyper::sync::InterruptSpinLock;

use super::super::NativeAddressSpace;
use {crate::task, crate::task::scheduler, crate::task::thread::ThreadId};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BusyDetachFailure {
    WrongTarget,
    Affinity,
    Timeout,
    NotObserved,
}

#[derive(Clone, Copy)]
struct State {
    owner: u64,
    thread: ThreadId,
    source: CpuIndex,
    destination: CpuIndex,
    cut_closed: bool,
    busy_seen: bool,
    failure: Option<BusyDetachFailure>,
}

static STATE: InterruptSpinLock<Option<State>, crate::hal::irq::LocalMask> =
    InterruptSpinLock::new(None);

/// The controller retains this guard through the synchronous mapping commit.
/// Its separate syscall gate must always be released before process teardown.
pub(crate) struct BusyDetachTest {
    owner: u64,
}

impl NativeAddressSpace {
    pub(crate) fn arm_busy_detach_for_test(
        &self,
        thread: ThreadId,
        source: CpuIndex,
        destination: CpuIndex,
    ) -> Option<BusyDetachTest> {
        if source == destination || thread == ThreadId::BOOTSTRAP {
            return None;
        }
        let owner = self.logical.id().get();
        STATE.with(|slot| {
            if slot.is_some() {
                return None;
            }
            *slot = Some(State {
                owner,
                thread,
                source,
                destination,
                cut_closed: false,
                busy_seen: false,
                failure: None,
            });
            Some(BusyDetachTest { owner })
        })
    }

    /// The selected callback waits with its setup pin until the updater has
    /// closed admission and requested migration. It releases the pin before
    /// explicitly reconsidering scheduling. No test pin crosses that switch.
    pub(crate) fn busy_cut_closed_for_test(thread: ThreadId) -> bool {
        STATE.with(|slot| {
            slot.as_ref()
                .is_some_and(|state| state.thread == thread && state.cut_closed)
        })
    }
}

impl BusyDetachTest {
    pub(crate) fn result(&self) -> Result<(), BusyDetachFailure> {
        STATE.with(|slot| {
            let state = slot
                .as_ref()
                .filter(|state| state.owner == self.owner)
                .ok_or(BusyDetachFailure::NotObserved)?;
            if let Some(failure) = state.failure {
                return Err(failure);
            }
            if state.cut_closed && state.busy_seen {
                Ok(())
            } else {
                Err(BusyDetachFailure::NotObserved)
            }
        })
    }
}

impl Drop for BusyDetachTest {
    fn drop(&mut self) {
        STATE.with(|slot| {
            if slot.as_ref().is_some_and(|state| state.owner == self.owner) {
                *slot = None;
            }
        });
    }
}

pub(in super::super) fn after_cut(owner: u64, active: &[bool; hyper::cpu::MAX_CPUS]) {
    let config = STATE.with(|slot| {
        slot.as_ref()
            .filter(|state| state.owner == owner && !state.cut_closed)
            .copied()
    });
    let Some(config) = config else {
        return;
    };
    // The transaction already holds its production preemption pin. The target
    // remains pinned in the syscall setup gate until cut_closed is published.
    let failure =
        if crate::cpu::current_index() == Some(config.source) || !active[config.source.get()] {
            Some(BusyDetachFailure::WrongTarget)
        } else if scheduler::set_thread_affinity(
            config.thread,
            scheduler::CpuMask::single(config.destination),
        )
        .is_err()
        {
            Some(BusyDetachFailure::Affinity)
        } else {
            None
        };
    STATE.with(|slot| {
        if let Some(state) = slot.as_mut().filter(|state| state.owner == owner) {
            state.failure = failure;
            state.cut_closed = true;
        }
    });
    if failure.is_some() {
        return;
    }
    let observed = crate::time::spin_wait_until(task::TEST_PROGRESS_TIMEOUT_NS, || {
        STATE.with(|slot| {
            slot.as_ref()
                .is_some_and(|state| state.owner == owner && state.busy_seen)
        })
    });
    if !matches!(observed, Ok(true)) {
        STATE.with(|slot| {
            if let Some(state) = slot.as_mut().filter(|state| state.owner == owner) {
                state.failure = Some(BusyDetachFailure::Timeout);
            }
        });
    }
    // Always continue the ordinary commit, including after a failed assertion.
    // Its RPC lets a target already waiting in leave make progress and cleanup
    // can then stop the process without leaving a closed residency cut behind.
}

pub(in super::super) fn observe_busy(owner: u64, cpu: CpuIndex) {
    STATE.with(|slot| {
        if let Some(state) = slot
            .as_mut()
            .filter(|state| state.owner == owner && state.source == cpu)
        {
            state.busy_seen = true;
        }
    });
}
