// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! End-to-end `AArch64` EL0 entry, Native syscall, and contained-fault proof.

use super::{Error, IMAGE_BASE, prepare_process, retire_process};
use hyper::mm::PAGE_SIZE;

use crate::task::scheduler::CpuMask;
use {crate::accounting::ResourceDomain, crate::accounting::ResourceLimits};
use {crate::capability::ResolvedWaitable, crate::capability::Rights};
use {
    crate::object::Event, crate::object::ObjectHandleState, crate::object::ObjectKind,
    crate::object::SignalWaitOutcome,
};
use {
    crate::process::MachineAbi, crate::process::Process, crate::process::TaskGroup,
    crate::process::TerminalReason, crate::process::UserThread,
};

const DIRECT_CALL_COUNT: usize = 66;

// Machine programs use generated object-local operations and real capabilities
// installed by prepare_process. The integer burst checks every result and an
// unknown method on a valid capability before reporting success with BRK #0.
macro_rules! native_program {
    ($function:ident, $start:ident, $end:ident) => {
        fn $function() -> &'static [u8] {
            unsafe extern "C" {
                static $start: u8;
                static $end: u8;
            }
            // SAFETY: These two immutable assembly labels delimit the named
            // position-independent program retained by the linker.
            unsafe { super::program_bytes(&raw const $start, &raw const $end) }
        }
    };
}

native_program!(
    program,
    aarch64_native_integer_start,
    aarch64_native_integer_end
);
native_program!(
    thread_exit_program,
    aarch64_native_thread_exit_start,
    aarch64_native_thread_exit_end
);
native_program!(
    process_exit_program,
    aarch64_native_process_exit_start,
    aarch64_native_process_exit_end
);
native_program!(
    event_wait_program,
    aarch64_native_event_wait_start,
    aarch64_native_event_wait_end
);
native_program!(
    cancelled_event_wait_program,
    aarch64_native_cancelled_event_wait_start,
    aarch64_native_cancelled_event_wait_end
);
native_program!(
    rollover_wait_program,
    aarch64_native_rollover_wait_start,
    aarch64_native_rollover_wait_end
);
native_program!(
    atomic_wait_program,
    aarch64_native_atomic_wait_start,
    aarch64_native_atomic_wait_end
);

#[derive(Clone, Copy)]
enum SiblingSetup {
    None,
    Dormant,
}

#[derive(Clone, Copy)]
enum RunControl {
    Join,
    CancelPublishedEventWait,
    CancelPublishedAtomicWait,
    ResumeAfterAsidRollover,
    ResumeAfterFileReclaim,
}

#[derive(Clone, Copy)]
enum ThreadAuthority {
    None,
    Publish,
    CloseDormant,
}

struct ProgramOutcome {
    thread: TerminalReason,
    process: TerminalReason,
    sibling: Option<TerminalReason>,
}

pub(crate) fn run() -> Result<(), Error> {
    let fp_counts = crate::hal::user::fp_state_counts_for_test();
    let runs = crate::hal::user::native_run_counts_for_test();
    let direct_calls = crate::hal::user::direct_native_call_count_for_test();
    let domain =
        ResourceDomain::try_new_root(ResourceLimits::UNLIMITED).map_err(|_| Error::Construction)?;
    let group = TaskGroup::try_new(&domain).map_err(|_| Error::Group)?;
    super::verify_thread_creation_affinity(&domain, &group, program(), MachineAbi::Aarch64)?;

    let outcome = run_program(
        &domain,
        &group,
        program(),
        "selftest/el0-fault",
        SiblingSetup::None,
        RunControl::Join,
        ThreadAuthority::Publish,
    )?;
    if !matches!(outcome.thread, TerminalReason::Fault { class: 6, code } if code & 0xffff == 0)
        || outcome.process != outcome.thread
        || outcome.sibling.is_some()
    {
        return Err(Error::Terminal);
    }
    if crate::hal::user::direct_native_call_count_for_test()
        != direct_calls.saturating_add(DIRECT_CALL_COUNT)
    {
        return Err(Error::Terminal);
    }

    if crate::hal::user::fp_state_counts_for_test() != fp_counts {
        return Err(Error::Terminal);
    }
    let after = crate::hal::user::native_run_counts_for_test();
    crate::pr_info!(
        "HypeR test: Native integer burst: returns={} machine-runs={} syscall-suspensions={} FP-copies=0",
        DIRECT_CALL_COUNT,
        after.0.saturating_sub(runs.0),
        after.1.saturating_sub(runs.1),
    );
    super::fp::run(&domain, &group)?;

    let outcome = run_program(
        &domain,
        &group,
        thread_exit_program(),
        "selftest/el0-thread-exit",
        SiblingSetup::None,
        RunControl::Join,
        ThreadAuthority::None,
    )?;
    if outcome.thread != (TerminalReason::ThreadExited { status: -17 })
        || outcome.process != (TerminalReason::LastThreadExited { status: -17 })
        || outcome.sibling.is_some()
    {
        return Err(Error::Terminal);
    }

    let outcome = run_program(
        &domain,
        &group,
        event_wait_program(),
        "selftest/el0-event-wait",
        SiblingSetup::None,
        RunControl::Join,
        ThreadAuthority::None,
    )?;
    if outcome.thread != (TerminalReason::ThreadExited { status: 0 })
        || outcome.process != (TerminalReason::LastThreadExited { status: 0 })
        || outcome.sibling.is_some()
    {
        return Err(Error::Terminal);
    }

    let outcome = run_program(
        &domain,
        &group,
        cancelled_event_wait_program(),
        "selftest/el0-event-cancel",
        SiblingSetup::None,
        RunControl::CancelPublishedEventWait,
        ThreadAuthority::None,
    )?;
    if outcome.thread != TerminalReason::Requested
        || outcome.process != TerminalReason::Requested
        || outcome.sibling.is_some()
    {
        return Err(Error::Terminal);
    }

    let outcome = run_program(
        &domain,
        &group,
        process_exit_program(),
        "selftest/el0-process-exit",
        SiblingSetup::Dormant,
        RunControl::Join,
        ThreadAuthority::None,
    )?;
    let process_exit = TerminalReason::ProcessExited { status: 23 };
    if outcome.thread != process_exit
        || outcome.process != process_exit
        || outcome.sibling != Some(process_exit)
    {
        return Err(Error::Terminal);
    }

    let outcome = run_program(
        &domain,
        &group,
        atomic_wait_program(),
        "selftest/el0-atomic-wait",
        SiblingSetup::None,
        RunControl::CancelPublishedAtomicWait,
        ThreadAuthority::Publish,
    )?;
    if outcome.thread != TerminalReason::Requested || outcome.process != TerminalReason::Requested {
        return Err(Error::Terminal);
    }

    let closed = run_program(
        &domain,
        &group,
        thread_exit_program(),
        "selftest/el0-abandoned",
        SiblingSetup::None,
        RunControl::Join,
        ThreadAuthority::CloseDormant,
    )?;
    if closed.thread != TerminalReason::Requested {
        return Err(Error::Terminal);
    }

    let rollover = run_program(
        &domain,
        &group,
        rollover_wait_program(),
        "selftest/el0-asid-rollover",
        SiblingSetup::None,
        RunControl::ResumeAfterAsidRollover,
        ThreadAuthority::None,
    )?;
    if rollover.thread != (TerminalReason::ThreadExited { status: 37 })
        || rollover.process != (TerminalReason::LastThreadExited { status: 37 })
    {
        return Err(Error::Terminal);
    }
    crate::pr_info!("HypeR test: Native execution resumed and retired after ASID rollover");

    let refault = run_program(
        &domain,
        &group,
        rollover_wait_program(),
        "selftest/el0-file-refault",
        SiblingSetup::None,
        RunControl::ResumeAfterFileReclaim,
        ThreadAuthority::None,
    )?;
    if refault.thread != (TerminalReason::ThreadExited { status: 37 })
        || refault.process != (TerminalReason::LastThreadExited { status: 37 })
    {
        return Err(Error::Terminal);
    }
    crate::pr_info!("HypeR test: Native execution refaulted after resident file-page retirement");

    super::atomic_wait::run(&domain, &group, program())?;
    verify_pending_creation_exit()?;

    group.request_stop().map_err(|_| Error::Group)?;
    group.finish_retirement().map_err(|_| Error::Group)?;
    Ok(())
}

fn run_program(
    domain: &ResourceDomain,
    group: &TaskGroup,
    program: &[u8],
    name: &str,
    sibling_setup: SiblingSetup,
    control: RunControl,
    thread_authority: ThreadAuthority,
) -> Result<ProgramOutcome, Error> {
    let process = prepare_process(domain, group, program, MachineAbi::Aarch64)?;
    let thread = process
        .create_initial_user_thread(name, CpuMask::ALL)
        .map_err(|_| Error::Construction)?;
    let object = thread.object_snapshot();
    if object.kind != ObjectKind::THREAD || object.handles != ObjectHandleState::Unpublished {
        return Err(Error::Construction);
    }
    let scheduler_id = match thread.scheduler_id() {
        Some(id) => id,
        None => return Err(Error::Construction),
    };
    let scheduler_object = crate::task::scheduler::thread_object_snapshot(scheduler_id)
        .map_err(|_| Error::Scheduler)?;
    if scheduler_object.object.koid != object.koid {
        // Scheduler execution and userspace authority must refer to one
        // canonical Thread object rather than parallel wrapper identities.
        return Err(Error::Construction);
    }
    let thread_waitable: Option<ResolvedWaitable> = match thread_authority {
        ThreadAuthority::None => None,
        ThreadAuthority::Publish | ThreadAuthority::CloseDormant => {
            let rights = Rights::WAIT.union(Rights::INSPECT);
            let handle = process
                .publish_thread_handle(&thread, rights)
                .map_err(|_| Error::Construction)?;
            let info = process
                .handle_info(handle, Rights::INSPECT)
                .map_err(|_| Error::Construction)?;
            let receiver = process
                .resolve_invocation(handle)
                .map_err(|_| Error::Construction)?;
            let resolved = UserThread::resolve_receiver(&receiver, Rights::INSPECT)
                .map_err(|_| Error::Construction)?;
            if info.kind != ObjectKind::THREAD
                || info.koid != object.koid
                || resolved.object_snapshot().koid != object.koid
            {
                return Err(Error::Construction);
            }
            if matches!(thread_authority, ThreadAuthority::CloseDormant) {
                process.close_handle(handle).map_err(|_| Error::Lifecycle)?;
                None
            } else {
                Some(
                    process
                        .resolve_waitable(handle, Rights::WAIT)
                        .map_err(|_| Error::Construction)?,
                )
            }
        }
    };
    let sibling = match sibling_setup {
        SiblingSetup::None => None,
        SiblingSetup::Dormant => Some(
            process
                .create_initial_user_thread("selftest/el0-dormant-sibling", CpuMask::ALL)
                .map_err(|_| Error::Construction)?,
        ),
    };
    if !matches!(thread_authority, ThreadAuthority::CloseDormant) {
        thread.ready().map_err(|_| Error::Scheduler)?;
    }
    if matches!(control, RunControl::CancelPublishedEventWait) {
        wait_for_event_registration(&process)?;
        let report = process.request_stop(TerminalReason::Requested);
        if !report.newly_requested || !report.dispatch_complete {
            return Err(Error::Lifecycle);
        }
    }
    if matches!(control, RunControl::CancelPublishedAtomicWait) {
        let ready =
            crate::task::wait_for_test_progress(crate::task::TEST_PROGRESS_TIMEOUT_NS, || {
                Ok::<_, Error>(
                    crate::process::atomic_wait::waiter_count(
                        &process,
                        IMAGE_BASE + PAGE_SIZE * 3 - 16,
                    )
                    .is_ok_and(|count| count == 1),
                )
            })?;
        if !ready {
            return Err(Error::Lifecycle);
        }
        // A repeated start cannot bypass a published atomic wait or enter the
        // scheduler's invariant-failure path. It must leave cancellation usable.
        if thread.ready().is_ok() {
            return Err(Error::Lifecycle);
        }
        let report = process.request_stop(TerminalReason::Requested);
        if !report.newly_requested || !report.dispatch_complete {
            return Err(Error::Lifecycle);
        }
    }
    if matches!(
        control,
        RunControl::ResumeAfterAsidRollover | RunControl::ResumeAfterFileReclaim
    ) {
        let result = (|| {
            wait_for_event_registration(&process)?;
            if matches!(control, RunControl::ResumeAfterFileReclaim) {
                // The Thread has executed this code and parked in the kernel.
                // Eviction must retire the resident CPU's translations before
                // returning; Event wakeup then faults on the same userspace PC.
                let address_space = process
                    .address_space_owner()
                    .map_err(|_| Error::AddressSpace)?;
                let code_range = crate::mm::user_space::UserSlice::new(
                    crate::mm::user_space::UserAddress::new(IMAGE_BASE),
                    PAGE_SIZE,
                )
                .map_err(|_| Error::AddressSpace)?;
                if address_space
                    .reclaim_file_range(code_range)
                    .map_err(|_| Error::AddressSpace)?
                    != 1
                {
                    return Err(Error::AddressSpace);
                }
            } else {
                let bits = crate::hal::user::address_space_plan()
                    .map_err(|_| Error::AddressSpace)?
                    .asid_bits();
                crate::mm::translation_id::test_rollover::<crate::mm::translation_id::HostAsid>(
                    bits,
                )
                .map_err(|_| Error::AddressSpace)?;
            }
            process
                .resolve_handle::<Event>(super::event_handle(&process)?, Rights::SIGNAL)
                .map_err(|_| Error::Lifecycle)?
                .object()
                .signal(0, Event::SIGNALED.bits())
                .map_err(|_| Error::Lifecycle)
        })();
        if let Err(error) = result {
            process.request_stop(TerminalReason::Requested);
            retire_process(&process)?;
            return Err(error);
        }
    }
    // Process completion is the externally visible aggregate lifetime cut.
    // Observe it first so this test also proves that every detached Thread has
    // published its terminal object signal before the Process reports stopped.
    let process_reason = process.join().map_err(|_| Error::Lifecycle)?;
    if let Some(waitable) = thread_waitable {
        match crate::object::wait_one(
            waitable.source(),
            &process.resource_domain(),
            crate::process::UserThread::TERMINATED.bits(),
            u64::MAX,
            || false,
        )
        .map_err(|_| Error::Lifecycle)?
        {
            SignalWaitOutcome::Observed(snapshot)
                if snapshot.signals() == crate::process::UserThread::TERMINATED => {}
            _ => return Err(Error::Lifecycle),
        }
    }
    let thread_reason = thread.join().map_err(|_| Error::Lifecycle)?;
    let sibling_reason = match sibling.as_ref() {
        Some(sibling) => Some(sibling.join().map_err(|_| Error::Lifecycle)?),
        None => None,
    };
    // Durable Thread objects are tombstones, not Process owners. Keeping both
    // observer references alive must not prevent Process/address-space
    // retirement after scheduler execution and membership have detached.
    retire_process(&process)?;
    drop(thread);
    drop(sibling);
    Ok(ProgramOutcome {
        thread: thread_reason,
        process: process_reason,
        sibling: sibling_reason,
    })
}

fn wait_for_event_registration(process: &Process) -> Result<(), Error> {
    if crate::task::wait_for_test_progress(crate::task::TEST_PROGRESS_TIMEOUT_NS, || {
        Ok::<_, Error>(super::event_handle(process).is_ok_and(|handle| {
            process
                .resolve_handle::<Event>(handle, Rights::WAIT)
                .is_ok_and(|event| event.object().waiter_count() == 1)
        }))
    })? {
        Ok(())
    } else {
        Err(Error::Lifecycle)
    }
}

// Hold the actual prepared-thread RAII owner while the last EL0 Thread exits.
// Its rollback must publish Process termination and let kreaper free the roots,
// even while the controller retains the Thread's diagnostic identity.
fn verify_pending_creation_exit() -> Result<(), Error> {
    // A private domain makes the baseline independent of asynchronous cleanup
    // from preceding probes. Only this test can change its accounting.
    let domain =
        ResourceDomain::try_new_root(ResourceLimits::UNLIMITED).map_err(|_| Error::Construction)?;
    let group = TaskGroup::try_new(&domain).map_err(|_| Error::Group)?;
    let baseline = domain.usage();
    for _ in 0..8 {
        let process = prepare_process(&domain, &group, thread_exit_program(), MachineAbi::Aarch64)?;
        let result = (|| {
            let thread = process
                .create_initial_user_thread("selftest/pending-exit", CpuMask::ALL)
                .map_err(|_| Error::Construction)?;
            let pending = process
                .prepare_thread_rollback_for_test()
                .map_err(|_| Error::Construction)?;
            thread.ready().map_err(|_| Error::Scheduler)?;
            let detached =
                crate::task::wait_for_test_progress(crate::task::TEST_PROGRESS_TIMEOUT_NS, || {
                    let snapshot = process.snapshot();
                    Ok::<_, Error>(snapshot.active_threads == 0 && snapshot.pending_threads == 1)
                })?;
            if !detached {
                return Err(Error::Lifecycle);
            }
            drop(pending);
            retire_process(&process)?;
            if process.snapshot().terminal != Some(TerminalReason::LastThreadExited { status: -17 })
                || thread.snapshot().terminal != Some(TerminalReason::ThreadExited { status: -17 })
            {
                return Err(Error::Terminal);
            }
            Ok(())
        })();
        // Also clean up if the regression fails: no admitted test work escapes.
        process.request_stop(TerminalReason::Requested);
        retire_process(&process)?;
        drop(process);
        let reclaimed =
            crate::task::wait_for_test_progress(crate::task::TEST_PROGRESS_TIMEOUT_NS, || {
                Ok::<_, Error>(domain.usage() == baseline)
            })?;
        result?;
        if !reclaimed {
            return Err(Error::Lifecycle);
        }
    }
    group.request_stop().map_err(|_| Error::Group)?;
    group.finish_retirement().map_err(|_| Error::Group)?;
    crate::pr_info!(
        "HypeR test: last-thread exit with creation rollback reclaimed resources (8 cycles)"
    );
    Ok(())
}
