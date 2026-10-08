// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Remote notification competes with cancellation/timeout while a wait is
//! Armed or crossing into park. All actors execute the real scheduler paths.

use hyper::cpu::CpuIndex;
use hyper::sync::InterruptSpinLock;
use hyper::sync::atomic::{AtomicUsize, Ordering};

use crate::kernel::task::scheduler::{self, CpuMask};
use crate::kernel::task::{WaitMobility, WaitOutcome, WaitQueue, WaitTicket};

const ROUNDS: usize = 32;
static TICKET: InterruptSpinLock<Option<WaitTicket>, crate::hal::irq::LocalMask> =
    InterruptSpinLock::new(None);
static QUEUE: WaitQueue = WaitQueue::new();
static PUBLISHED: AtomicUsize = AtomicUsize::new(0);
static START: AtomicUsize = AtomicUsize::new(0);
static QUEUED: AtomicUsize = AtomicUsize::new(0);
static READY: [AtomicUsize; 2] = [const { AtomicUsize::new(0) }; 2];
static DONE: [AtomicUsize; 2] = [const { AtomicUsize::new(0) }; 2];
static WINNERS: AtomicUsize = AtomicUsize::new(0);
static MADE_READY: AtomicUsize = AtomicUsize::new(0);
static COMMITS: AtomicUsize = AtomicUsize::new(0);
static FAILURE: AtomicUsize = AtomicUsize::new(0);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Error {
    Scheduler(scheduler::Error),
    Quiescence(super::support::QuiescenceError),
    Progress { round: usize, phase: &'static str },
    Outcome { round: usize, winners: usize },
}

impl From<scheduler::Error> for Error {
    fn from(error: scheduler::Error) -> Self {
        Self::Scheduler(error)
    }
}

pub(super) fn run() -> Result<(), Error> {
    if crate::kernel::cpu::online_cpu_count() < 3 {
        crate::pr_info!("HypeR test: concurrent wait arbitration skipped (requires three CPUs)");
        return Ok(());
    }
    super::support::quiesce_workers().map_err(Error::Quiescence)?;
    for actor in 0..2 {
        let cpu = CpuIndex::new(actor + 1).ok_or(Error::Outcome {
            round: 0,
            winners: 0,
        })?;
        let worker = scheduler::kthread_create_with_affinity(
            "wait/competing-resolver",
            resolver,
            actor,
            CpuMask::single(cpu),
        )?;
        scheduler::thread_ready(worker)?;
    }
    for round in 1..=ROUNDS {
        exercise_round(round)?;
    }
    // Completion counters do not prove thread/stack retirement. Wait for the
    // actual scheduler reaper before the next self-test reuses resources.
    super::support::quiesce_workers().map_err(Error::Quiescence)?;
    crate::pr_info!("HypeR test: concurrent wait arbitration passed (32 rounds)");
    Ok(())
}

fn exercise_round(round: usize) -> Result<(), Error> {
    WINNERS.store(0, Ordering::Relaxed);
    MADE_READY.store(0, Ordering::Relaxed);
    COMMITS.store(0, Ordering::Relaxed);
    let registration = scheduler::begin_wait(WaitMobility::Migratable)?;
    let ticket = registration.ticket();
    TICKET.with(|slot| *slot = Some(ticket));
    PUBLISHED.store(round, Ordering::Release);
    if let Err(error) = wait_until(round, "resolvers ready", || {
        READY
            .iter()
            .all(|ready| ready.load(Ordering::Acquire) == round)
    }) {
        scheduler::finish_wait(registration)?;
        return Err(error);
    }
    START.store(round, Ordering::Release);

    let completed = || {
        DONE.iter()
            .all(|done| done.load(Ordering::Acquire) == round)
    };
    let outcome = if round.is_multiple_of(2) {
        // Keep this registration Armed until both remote resolvers finish.
        let progress = wait_until(round, "armed resolution", completed);
        let outcome = scheduler::finish_wait(registration)?;
        progress?;
        outcome
    } else {
        // Resolvers wait for queue publication, then race the switch tail or
        // the parked continuation through the production scheduler paths.
        let outcome = scheduler::prepare_registered_park(&QUEUE, registration)?.complete();
        wait_until(round, "park resolution", completed)?;
        Some(outcome)
    };

    let winners = WINNERS.load(Ordering::Relaxed);
    let expected = match winners {
        1 => Some(WaitOutcome::Notified),
        2 => Some(competing_outcome(round)),
        _ => None,
    };
    if expected.is_none()
        || outcome != expected
        || COMMITS.load(Ordering::Relaxed) != usize::from(winners == 1)
        || MADE_READY.load(Ordering::Relaxed) != usize::from(!round.is_multiple_of(2))
        || !QUEUE.is_empty()?
    {
        return Err(Error::Outcome { round, winners });
    }
    // A late notification cannot execute its callback after wait retirement.
    let replay = scheduler::notify_registered_with(ticket, || {
        COMMITS.fetch_add(1, Ordering::Relaxed);
    })?;
    if replay.won
        || replay.made_ready
        || COMMITS.load(Ordering::Relaxed) != usize::from(winners == 1)
    {
        return Err(Error::Outcome { round, winners });
    }
    Ok(())
}

fn competing_outcome(round: usize) -> WaitOutcome {
    if round % 4 < 2 {
        WaitOutcome::Cancelled
    } else {
        WaitOutcome::TimedOut
    }
}

extern "C" fn resolver(actor: usize) {
    for round in 1..=ROUNDS {
        let result = (|| {
            wait_until(round, "ticket publication", || {
                PUBLISHED.load(Ordering::Acquire) == round
            })?;
            let ticket = TICKET
                .with(|slot| *slot)
                .ok_or(Error::Outcome { round, winners: 0 })?;
            READY[actor].store(round, Ordering::Release);
            wait_until(round, "start", || START.load(Ordering::Acquire) == round)?;
            if !round.is_multiple_of(2) {
                wait_until(round, "queue publication", || {
                    // Latch the observation: the winner may dequeue the wait
                    // before the other resolver gets to inspect the queue.
                    if QUEUED.load(Ordering::Acquire) == round {
                        return true;
                    }
                    if QUEUE.len() == Ok(1) {
                        QUEUED.store(round, Ordering::Release);
                        return true;
                    }
                    false
                })?;
            }
            let result = if actor == 0 {
                scheduler::notify_registered_with(ticket, || {
                    COMMITS.fetch_add(1, Ordering::Relaxed);
                })?
            } else {
                scheduler::resolve_wait(ticket, competing_outcome(round))?
            };
            if result.won {
                WINNERS.fetch_or(1 << actor, Ordering::Relaxed);
            }
            if result.made_ready {
                MADE_READY.fetch_add(1, Ordering::Relaxed);
            }
            DONE[actor].store(round, Ordering::Release);
            Ok::<_, Error>(())
        })();
        if let Err(error) = result {
            FAILURE.store(actor + 1, Ordering::Release);
            crate::pr_err!("HypeR test: wait resolver {actor}, round {round}: {error:?}");
            return;
        }
    }
}

// The controller may own an Armed registration, so it must not start another
// timed wait. Actors have dedicated CPUs, hold no lock here, and keep IRQs on.
fn wait_until(round: usize, phase: &'static str, ready: impl Fn() -> bool) -> Result<(), Error> {
    let start = crate::kernel::time::monotonic_microseconds();
    while !ready() {
        if FAILURE.load(Ordering::Acquire) != 0
            || crate::kernel::time::monotonic_microseconds().saturating_sub(start) > 4_000_000
        {
            return Err(Error::Progress { round, phase });
        }
        core::hint::spin_loop();
    }
    Ok(())
}
