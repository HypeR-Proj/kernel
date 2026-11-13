// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Deterministic enrollment races through the production source arbiter.

use super::*;
use crate::task::{WaitOutcome, WaitTicket};

#[derive(Clone, Copy, Debug)]
enum Precursor {
    None,
    Timeout,
    ThreadCancellation,
    EventCancellation,
}

#[derive(Clone, Copy, Debug)]
enum Transition {
    Mismatch,
    OwnerExit,
}

pub(super) fn run(state: &State) -> Result<(), Error> {
    for precursor in [
        Precursor::Timeout,
        Precursor::ThreadCancellation,
        Precursor::EventCancellation,
        Precursor::None,
    ] {
        for transition in [Transition::Mismatch, Transition::OwnerExit] {
            case(state, precursor, transition, false)?;
        }
    }
    // A real future timer must be cancelled and retired without manufacturing a
    // cancellation result when the second condition check completes the wait.
    case(state, Precursor::None, Transition::Mismatch, true)?;
    store(state, state.expected)?;
    Ok(())
}

fn case(
    state: &State,
    precursor: Precursor,
    transition: Transition,
    future_timer: bool,
) -> Result<(), Error> {
    let account = state.process.resource_domain();
    let baseline = account.usage();
    let owner = state
        .process
        .create_initial_user_thread("sync-domain/admission-owner", scheduler::CpuMask::ALL)
        .map_err(|_| Error::Construction)?;
    let result = (|| {
        let token =
            SyncDomain::join(&state.domain, &owner, &account).map_err(|_| Error::Construction)?;
        store(state, token | WAITERS)?;
        let word = Word::prepare(
            &state.process,
            state.source.clone(),
            state.domain.koid().get(),
            0,
        )
        .map_err(|_| Error::Construction)?;
        let cancellation = PreparedCancellation::new(state.cancellation.clone(), &account)
            .map_err(|_| Error::Construction)?;
        let deadline = if future_timer {
            crate::time::monotonic_nanoseconds()
                .map_err(|_| Error::Construction)?
                .checked_add(60_000_000_000)
                .ok_or(Error::Construction)?
        } else {
            u64::MAX
        };
        let mut hook_result = Ok(());
        let actual = sync_wait::wait_with_admission_hook(
            &state.process,
            state.domain.clone(),
            word,
            request(state, token | WAITERS, token, deadline),
            Some(cancellation),
            || false,
            |ticket| {
                hook_result = (|| {
                    resolve_precursor(state, ticket, precursor)?;
                    match transition {
                        Transition::Mismatch => store(state, 0),
                        Transition::OwnerExit => detach_during_admission(&owner),
                    }
                })();
                if hook_result.is_err() {
                    // Retire the outer wait before propagating a fixture error.
                    let _ = scheduler::resolve_wait(ticket, WaitOutcome::Cancelled);
                }
            },
        );
        hook_result?;
        let actual = actual.map_err(|_| Error::AtomicWait("admission race wait"))?;
        let wanted = match precursor {
            Precursor::Timeout => Outcome::TimedOut,
            Precursor::ThreadCancellation | Precursor::EventCancellation => Outcome::Cancelled,
            Precursor::None => match transition {
                Transition::Mismatch => Outcome::Recheck,
                Transition::OwnerExit => Outcome::OwnerExited(token),
            },
        };
        if actual != wanted {
            crate::pr_err!(
                "SyncDomain admission race {precursor:?}/{transition:?}: {actual:?}, expected {wanted:?}"
            );
            return Err(Error::AtomicWait("admission overwrote source winner"));
        }
        Ok(())
    })();
    state
        .cancellation
        .object()
        .signal(Event::SIGNALED.bits(), 0)
        .map_err(|_| Error::Construction)?;
    if owner.try_join().is_none() {
        stop(&owner)?;
    }
    drop(owner);
    if !wait_for_test_progress(TEST_PROGRESS_TIMEOUT_NS, || {
        Ok::<_, Error>(account.usage() == baseline)
    })? {
        return Err(Error::AtomicWait("admission source retirement"));
    }
    result
}

fn resolve_precursor(state: &State, ticket: WaitTicket, precursor: Precursor) -> Result<(), Error> {
    let outcome = match precursor {
        Precursor::None => return Ok(()),
        Precursor::EventCancellation => {
            state
                .cancellation
                .object()
                .signal(0, Event::SIGNALED.bits())
                .map_err(|_| Error::Construction)?;
            return Ok(());
        }
        Precursor::Timeout => WaitOutcome::TimedOut,
        Precursor::ThreadCancellation => WaitOutcome::Cancelled,
    };
    // Exercise production ticket arbitration, not timer hardware: injected
    // timeout cases use an infinite deadline and therefore own no real timer.
    let resolution = scheduler::resolve_wait(ticket, outcome).map_err(|_| Error::Scheduler)?;
    if !resolution.won || resolution.made_ready {
        return Err(Error::AtomicWait("admission precursor did not win"));
    }
    Ok(())
}

fn detach_during_admission(owner: &UserThread) -> Result<(), Error> {
    scheduler::request_user_thread_stop(
        owner.scheduler_id().ok_or(Error::Construction)?,
        TerminalReason::Requested,
    )
    .map_err(|_| Error::Scheduler)?;
    let deadline =
        crate::time::deadline_after(TEST_PROGRESS_TIMEOUT_NS).map_err(|_| Error::Construction)?;
    while owner.try_join().is_none() {
        if hyper::hal::timer::deadline_reached(crate::time::monotonic_ticks(), deadline) {
            return Err(Error::AtomicWait("admission owner actual detach"));
        }
        // The outer ticket is already armed. Yield without creating a nested
        // sleep ticket while the production Thread detach publishes death.
        scheduler::yield_now().map_err(|_| Error::Scheduler)?;
    }
    Ok(())
}
