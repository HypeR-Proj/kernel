// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Real filesystem Health and peer-close sources share one scheduler ticket.
//! Full file/page mapping coverage lives in the Native filesystem runtime test.

use super::*;
use crate::authority::Rights;
use crate::fs::vfs::FileFailureTestControl;
use crate::ipc::ByteChannel;
use crate::object::{KernelRef, KernelService, TimedWaitPreparation, prepare_timed_wait};
use crate::task::{WaitQueue, WaitTicket};
use hyper::mm::FallibleArc;

#[derive(Clone, Copy)]
enum First {
    Local,
    LocalClose,
    Peer,
    Timeout,
    Cancel,
    Wake,
}

struct Fixture {
    control: FileFailureTestControl,
    peer: ByteChannel,
    peer_closed: AtomicBool,
    channel: KernelRef<ByteChannel, KernelService>,
    handle: crate::capability::HandleValue,
    local_closed: AtomicBool,
}

impl Fixture {
    fn new(process: &Process) -> Result<Self, Error> {
        let sponsor = ResourceDomain::try_new_root(ResourceLimits::UNLIMITED)
            .map_err(|_| Error::Construction)?;
        let (client, peer) = ByteChannel::try_pair(&sponsor).map_err(|_| Error::Construction)?;
        let handle = process
            .create_object(client, Rights::WAIT)
            .map_err(|_| Error::Construction)?;
        let channel = process
            .resolve_handle::<ByteChannel>(handle, Rights::WAIT)
            .map_err(|_| Error::Construction)?
            .into_operation_pin()
            .retain_service();
        let control = FileFailureTestControl::new(channel.clone(), &sponsor)
            .map_err(|_| Error::Construction)?;
        Ok(Self {
            control,
            peer,
            peer_closed: AtomicBool::new(false),
            channel,
            handle,
            local_closed: AtomicBool::new(false),
        })
    }

    fn close_peer(&self) {
        // Injection and cleanup share this fixture; production endpoint close
        // is a one-shot transition rather than an idempotent notification.
        if !self.peer_closed.swap(true, Ordering::AcqRel) {
            self.peer.close_for_test();
        }
    }

    fn close_local(&self, process: &Process) -> Result<(), Error> {
        if !self.local_closed.swap(true, Ordering::AcqRel) {
            process
                .close_handle(self.handle)
                .map_err(|_| Error::Lifecycle)?;
        }
        Ok(())
    }

    fn fire(&self, process: &Process, first: First, ticket: WaitTicket) -> Result<(), Error> {
        match first {
            First::Local => self.control.fail(),
            First::LocalClose => self.close_local(process)?,
            First::Peer => self.close_peer(),
            First::Timeout | First::Cancel => {
                let outcome = if matches!(first, First::Timeout) {
                    WaitOutcome::TimedOut
                } else {
                    WaitOutcome::Cancelled
                };
                scheduler::resolve_wait(ticket, outcome).map_err(|_| Error::Scheduler)?;
            }
            First::Wake => {
                scheduler::notify_registered_with(ticket, || {}).map_err(|_| Error::Scheduler)?;
            }
        }
        Ok(())
    }
}

pub(super) fn run(process: &Process) -> Result<(), Error> {
    for first in [
        First::Local,
        First::LocalClose,
        First::Peer,
        First::Timeout,
        First::Cancel,
        First::Wake,
    ] {
        admission(process, first, false)?;
    }
    // A retained transport keeps identity, not authority: registration after
    // final local-handle closure must observe failure while its peer stays open.
    admission(process, First::LocalClose, true)?;
    for cause in [First::Local, First::LocalClose, First::Peer] {
        parked(process, cause)?;
    }
    crate::pr_info!(
        "HypeR test: File wait failure sources preserve winners and retire parked waits"
    );
    Ok(())
}

fn armed(account: &ResourceDomain) -> Result<crate::object::PreparedTimedWait, Error> {
    match prepare_timed_wait(account, u64::MAX).map_err(|_| Error::Construction)? {
        TimedWaitPreparation::Armed(wait) => Ok(wait),
        _ => Err(Error::Construction),
    }
}

fn admission(
    process: &Process,
    first: First,
    close_before_registration: bool,
) -> Result<(), Error> {
    let fixture = Fixture::new(process)?;
    // Source accounting is independent of the process handle charge, which
    // legitimately decreases in the local-endpoint closure cases.
    let account =
        ResourceDomain::try_new_root(ResourceLimits::UNLIMITED).map_err(|_| Error::Construction)?;
    let baseline = account.usage();
    let result = (|| {
        if close_before_registration {
            fixture.close_local(process)?;
        }
        let sources = fixture
            .control
            .prepare(&account)
            .map_err(|_| Error::Construction)?;
        let wait = armed(&account)?;
        let ticket = wait.ticket();
        let sources = sources.register(ticket);
        let fired = if close_before_registration {
            Ok(())
        } else {
            fixture.fire(process, first, ticket)
        };
        // Probe before firing other causes: otherwise a missing local-close
        // subscription could be concealed by the following explicit poison.
        let retained_winner = matches!(
            scheduler::resolve_wait(ticket, WaitOutcome::Cancelled),
            Ok(result) if !result.won
        );
        // All late failure causes must be clean losers when timer, cancel or
        // normal notification has already selected this generation.
        fixture.control.fail();
        fixture.close_peer();
        let closed = fixture.close_local(process);
        let outcome = wait.complete_unpublished();
        let failed = sources.finish();
        fired?;
        closed?;
        let wanted = match first {
            First::Timeout => WaitOutcome::TimedOut,
            First::Cancel => WaitOutcome::Cancelled,
            _ => WaitOutcome::Notified,
        };
        if !retained_winner
            || outcome.map_err(|_| Error::Construction)? != wanted
            || failed != matches!(first, First::Local | First::LocalClose | First::Peer)
            || fixture.channel.object().signal_waiter_count_for_test() != 0
            || fixture.peer.signal_waiter_count_for_test() != 0
            || account.usage() != baseline
        {
            return Err(Error::AtomicWait(
                "file failure replaced earlier source or retained registration",
            ));
        }
        // Reusing the same Thread must observe the sticky failure under the new
        // ticket, never an orphaned notification from the previous generation.
        let sources = fixture
            .control
            .prepare(&account)
            .map_err(|_| Error::Construction)?;
        let wait = armed(&account)?;
        let sources = sources.register(wait.ticket());
        let outcome = wait.complete_unpublished();
        let failed = sources.finish();
        if outcome.map_err(|_| Error::Construction)? != WaitOutcome::Notified
            || !failed
            || account.usage() != baseline
        {
            return Err(Error::AtomicWait("file failure level or ticket reuse"));
        }
        Ok(())
    })();
    fixture.close_peer();
    let close = fixture.close_local(process);
    result.and(close)
}

struct Parked {
    fixture: Fixture,
    account: ResourceDomain,
    queue: InterruptSpinLock<WaitQueue, crate::hal::irq::LocalMask>,
    result: AtomicUsize,
}
static PARKED: InterruptSpinLock<Option<FallibleArc<Parked>>, crate::hal::irq::LocalMask> =
    InterruptSpinLock::new(None);

fn parked(process: &Process, cause: First) -> Result<(), Error> {
    let state = FallibleArc::try_new(Parked {
        fixture: Fixture::new(process)?,
        account: process.resource_domain(),
        queue: InterruptSpinLock::new(WaitQueue::new()),
        result: AtomicUsize::new(0),
    })
    .map_err(|_| Error::Construction)?;
    PARKED.with(|slot| *slot = Some(state.clone()));
    let result = (|| {
        let id = scheduler::kthread_create("atomic-wait/file-failure", failure_worker, 0)
            .map_err(|_| Error::Scheduler)?;
        if !matches!(scheduler::thread_ready(id), Ok(true)) {
            scheduler::discard_dormant_kernel_thread(id).map_err(|_| Error::Scheduler)?;
            return Err(Error::Scheduler);
        }
        if !wait_for_test_progress(TEST_PROGRESS_TIMEOUT_NS, || {
            state
                .queue
                .with(|queue| scheduler::waiter_count(queue))
                .map(|count| count == 1 || state.result.load(Ordering::Acquire) != 0)
                .map_err(|_| Error::Scheduler)
        })? || state.result.load(Ordering::Acquire) != 0
        {
            return Err(Error::AtomicWait("file failure waiter did not park"));
        }
        match cause {
            First::Peer => state.fixture.close_peer(),
            First::LocalClose => state.fixture.close_local(process)?,
            First::Local => state.fixture.control.fail(),
            _ => return Err(Error::AtomicWait("invalid parked failure fixture")),
        }
        if !wait_for_test_progress(TEST_PROGRESS_TIMEOUT_NS, || {
            Ok::<_, Error>(state.result.load(Ordering::Acquire) != 0)
        })? || state.result.load(Ordering::Acquire) != 1
        {
            return Err(Error::AtomicWait(
                "file failure did not retire parked waiter",
            ));
        }
        Ok(())
    })();
    state.fixture.control.fail();
    state.fixture.close_peer();
    crate::kernel_tests::support::quiesce_workers().map_err(|_| Error::Scheduler)?;
    drop(PARKED.with(Option::take));
    let close = state.fixture.close_local(process);
    result.and(close)
}

extern "C" fn failure_worker(_: usize) {
    let Some(state) = PARKED.with(|slot| slot.clone()) else {
        return;
    };
    let result = (|| {
        let sources = state
            .fixture
            .control
            .prepare(&state.account)
            .map_err(|_| Error::Construction)?;
        let wait = armed(&state.account)?;
        let sources = sources.register(wait.ticket());
        // SAFETY: the stable condition queue remains owned by state until worker
        // quiescence. Signals already participate in this exact armed ticket.
        let (published, mask) = unsafe {
            state
                .queue
                .with_mask_retained(|queue| wait.publish_locked(queue))
        };
        let (outcome, retirement) = published.finish(mask);
        let failed = sources.finish();
        retirement.map_err(|_| Error::Construction)?;
        if outcome != WaitOutcome::Notified || !failed {
            return Err(Error::AtomicWait("parked failure outcome"));
        }
        Ok(())
    })();
    state
        .result
        .store(if result.is_ok() { 1 } else { 2 }, Ordering::Release);
}
