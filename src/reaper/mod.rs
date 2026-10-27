// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Normal-context retirement service for detached kernel resources.
//!
//! Producers publish work without allocating or acquiring scheduler policy
//! locks. The dedicated worker drains subsystem retirement before final
//! kernel-object destruction, and the shared sticky-work protocol closes the
//! queue-empty-to-sleep race for both sources.

use core::sync::atomic::{AtomicBool, Ordering};

use hyper::sync::{DeferredWork, PublishedOnce, WorkDisposition};

#[cfg(all(
    all(feature = "kernel-test", feature = "kernel-self-test"),
    CONFIG_ARCH_AARCH64
))]
pub(crate) mod retry_test;

const REAP_BATCH: usize = 16;
const RETIREMENT_RETRY_NS: u64 = 10_000_000;

static WORK: DeferredWork = DeferredWork::new();
static WAKE: crate::sync::Completion = crate::sync::Completion::new();
static WORKER_PUBLISHED: AtomicBool = AtomicBool::new(false);
static IRQ_PROMPTS_READY: AtomicBool = AtomicBool::new(false);
// The permanent callout owns its callback context. Its timer worker requests
// retirement work without sharing the reaper's stack or lifecycle token.
static RETRY_TIMER: PublishedOnce<crate::time::Callout<()>> = PublishedOnce::new();
static RETRY_DUE: AtomicBool = AtomicBool::new(false);

#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
static TEST_WORKER: hyper::sync::InterruptSpinLock<
    Option<crate::task::thread::ThreadId>,
    crate::hal::irq::LocalMask,
> = hyper::sync::InterruptSpinLock::new(None);
#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
static TEST_RETRY_CPU: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(usize::MAX);

// The current process-retirement integration case needs Native user mappings,
// so only the AArch64 self-test binary calls these architecture-neutral hooks.
#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
#[allow(dead_code)]
pub(crate) fn set_affinity_for_test(
    affinity: crate::task::scheduler::CpuMask,
) -> Result<(), crate::task::scheduler::Error> {
    let worker = TEST_WORKER
        .with(|worker| *worker)
        .ok_or(crate::task::scheduler::Error::NotInitialized)?;
    crate::task::scheduler::set_thread_affinity(worker, affinity)?;
    TEST_RETRY_CPU.store(usize::MAX, Ordering::Release);
    Ok(())
}

#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
#[allow(dead_code)]
pub(crate) fn retry_observed_on_for_test(cpu: hyper::cpu::CpuIndex) -> bool {
    TEST_RETRY_CPU.load(Ordering::Acquire) == cpu.get()
}

/// Unforgeable proof that finalizers execute on the dedicated reaper Thread.
pub(crate) struct ReaperAccess {
    _private: (),
}

/// Creates and publishes the sole normal-context retirement worker.
pub(crate) fn initialize() -> Result<(), crate::task::scheduler::Error> {
    let worker = crate::task::scheduler::kthread_create("kreaper", worker_entry, 0)?;
    if !WORK.claim_initial_worker()
        || WORKER_PUBLISHED
            .compare_exchange(false, true, Ordering::Release, Ordering::Relaxed)
            .is_err()
    {
        return Err(crate::task::scheduler::Error::AlreadyInitialized);
    }
    #[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
    TEST_WORKER.with(|slot| *slot = Some(worker));
    // The initial worker owns every request published before this point.
    // Clear their unissued prompt before the worker can drain and sleep, or a
    // stale IRQ_PROMPTED bit could suppress the first post-startup notifier.
    let _ = WORK.consume_prompt();
    if !crate::task::scheduler::thread_ready(worker)? {
        return Err(crate::task::scheduler::Error::InvalidThreadState);
    }
    Ok(())
}

/// Publishes retirement work and prompts IRQ-tail wakeup when required.
///
/// Requests made before worker publication remain sticky. The initial worker
/// owns their first drain, so early object construction rollback needs no
/// scheduler or interrupt dependency.
pub(crate) fn request() {
    let elected = WORK.request();
    if !elected || !IRQ_PROMPTS_READY.load(Ordering::Acquire) {
        return;
    }
    match crate::cpu::current_index() {
        Some(cpu) => crate::irq::reschedule::notify(cpu),
        // IRQ prompting is enabled only after bootstrap CPU identity and the
        // interrupt route exist. A plain event would wake a WFI but would not
        // consume the durable prompt or wake the scheduler Thread.
        None => hyper::debug::invariant_failure("reaper::request invariant"),
    }
}

/// Opens hardware prompting after the interrupt subsystem is operational.
///
/// Work published between scheduler and IRQ initialization remains in the
/// sticky state. This transition consumes any elected early prompt and, when
/// the worker does not already own wake responsibility, completes its wait
/// directly from normal boot context.
pub(crate) fn enable_irq_prompts() {
    if !WORKER_PUBLISHED.load(Ordering::Acquire)
        || IRQ_PROMPTS_READY
            .compare_exchange(false, true, Ordering::Release, Ordering::Relaxed)
            .is_err()
    {
        hyper::debug::invariant_failure("reaper::enable_irq_prompts invariant");
    }
    if WORK.consume_prompt()
        && WORK.claim_notification()
        && let Err(error) = WAKE.complete()
    {
        crate::diagnostics::crash::fatal(format_args!(
            "HypeR: initial kernel reaper wake failed: {error:?}"
        ));
    }
}

/// Converts one durable hardware prompt into a scheduler-safe worker wake.
pub(crate) fn service_irq_prompt() {
    if !WORK.consume_prompt() || !WORK.claim_notification() {
        return;
    }
    if let Err(error) = WAKE.complete() {
        crate::diagnostics::crash::fatal(format_args!(
            "HypeR: kernel reaper wake failed: {error:?}"
        ));
    }
}

extern "C" fn worker_entry(_argument: usize) {
    let mut access = ReaperAccess { _private: () };
    let mut prefer_objects = false;
    // Allocate once: retries under memory pressure do not need another node.
    // Ordinary callback delivery happens on the source CPU's timer worker.
    let retry_timer = match crate::time::Callout::try_new((), retry_expired) {
        Ok(timer) => timer,
        Err(error) => crate::diagnostics::crash::fatal(format_args!(
            "HypeR: reaper timer reservation failed: {error:?}"
        )),
    };
    if RETRY_TIMER.publish(retry_timer).is_err() {
        hyper::debug::invariant_failure("reaper retry timer published twice");
    }
    let retry_timer = match RETRY_TIMER.get() {
        Some(timer) => timer,
        None => hyper::debug::invariant_failure("reaper retry timer missing after publication"),
    };
    let mut retry_pending = false;
    loop {
        // Clear the previous batch's request before inspecting any producer
        // condition. A retry racing the condition check must remain sticky;
        // clearing WORK afterward could erase its sole wake responsibility.
        WORK.begin_batch();
        // Unrelated retirement work may wake the worker while a delayed retry
        // remains delayed. Preserve that timer so a steady producer stream
        // cannot postpone older Process or VM retirement indefinitely.
        if RETRY_DUE.swap(false, Ordering::AcqRel) {
            if !retry_pending {
                crate::diagnostics::crash::fatal(format_args!(
                    "HypeR: retirement retry expired without timer ownership"
                ));
            }
            // Publishing RETRY_DUE can wake this Thread before the callback
            // returns. Join it before making the reservation available again.
            if let Err(error) = retry_timer.drain() {
                crate::diagnostics::crash::fatal(format_args!(
                    "HypeR: reaper timer retirement failed: {error:?}"
                ));
            }
            retry_pending = false;
            crate::process::promote_delayed_retirements(&mut access);
        }
        #[cfg(all(
            all(feature = "kernel-test", feature = "kernel-self-test"),
            CONFIG_ARCH_AARCH64
        ))]
        retry_test::pause_after_retry_check(retry_pending);
        crate::process::reap_one_process(&mut access);
        let first_vm_pass = crate::vm::lifecycle::reap_batch(&mut access);
        let mut subsystem_work = false;
        let mut subsystem_progress = false;
        for _ in 0..REAP_BATCH {
            let result = if prefer_objects {
                reap_object_or_thread(&mut access)
            } else {
                reap_thread_or_object(&mut access)
            };
            prefer_objects = !prefer_objects;
            match result {
                Ok(ReapStep::Reaped { more }) => {
                    subsystem_progress = true;
                    subsystem_work = more;
                }
                Ok(ReapStep::Empty) => {
                    subsystem_work = false;
                    break;
                }
                Err(error) => crate::diagnostics::crash::fatal(format_args!(
                    "HypeR: kernel reaper failed: {error:?}"
                )),
            }
        }
        // A vCPU Thread reaped above may have completed the exact endpoint
        // which blocked the first VM pass. Revisit each remaining VM once so
        // that completion does not pay an unconditional retry interval.
        let vm_retirement = if first_vm_pass.needs_retry && subsystem_progress {
            crate::vm::lifecycle::reap_batch(&mut access)
        } else {
            first_vm_pass
        };
        let process_work = crate::process::retirement_work(&access);
        if (process_work.delayed || vm_retirement.needs_retry) && !retry_pending {
            let deadline = match crate::time::deadline_after(RETIREMENT_RETRY_NS) {
                Ok(deadline) => deadline,
                Err(error) => crate::diagnostics::crash::fatal(format_args!(
                    "HypeR: reaper retry deadline failed: {error:?}"
                )),
            };
            if let Err(error) = retry_timer.arm_at(deadline) {
                crate::diagnostics::crash::fatal(format_args!(
                    "HypeR: reaper retry timer failed: {error:?}"
                ));
            }
            retry_pending = true;
        }
        let more_work =
            subsystem_work || process_work.ready || vm_retirement.continue_immediately();
        // Arm before relinquishing work ownership. An expiry or producer racing
        // this handoff sets durable work and cannot be lost before WAKE.wait.
        match WORK.finish_batch(more_work) {
            WorkDisposition::Continue => {
                if let Err(error) = crate::task::scheduler::cond_resched() {
                    crate::diagnostics::crash::fatal(format_args!(
                        "HypeR: kernel reaper reschedule failed: {error:?}"
                    ));
                }
            }
            WorkDisposition::Wait => {
                if let Err(error) = WAKE.wait() {
                    crate::diagnostics::crash::fatal(format_args!(
                        "HypeR: kernel reaper wait failed: {error:?}"
                    ));
                }
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReapStep {
    Reaped { more: bool },
    Empty,
}

fn reap_thread_or_object(
    access: &mut ReaperAccess,
) -> Result<ReapStep, crate::task::scheduler::Error> {
    match crate::task::scheduler::reap_one_thread(access)? {
        Some(more) => Ok(ReapStep::Reaped {
            more: more || crate::object::final_reap_pending(access),
        }),
        None => Ok(reap_one_object(access)),
    }
}

fn reap_object_or_thread(
    access: &mut ReaperAccess,
) -> Result<ReapStep, crate::task::scheduler::Error> {
    if crate::object::reap_one_final_object(access) {
        return Ok(ReapStep::Reaped {
            more: crate::object::final_reap_pending(access)
                || crate::task::scheduler::retirement_pending(access),
        });
    }
    match crate::task::scheduler::reap_one_thread(access)? {
        Some(more) => Ok(ReapStep::Reaped { more }),
        None => Ok(ReapStep::Empty),
    }
}

fn reap_one_object(access: &mut ReaperAccess) -> ReapStep {
    if crate::object::reap_one_final_object(access) {
        ReapStep::Reaped {
            more: crate::object::final_reap_pending(access),
        }
    } else {
        ReapStep::Empty
    }
}

fn retry_expired(
    _context: &(),
    _callout: crate::time::CalloutRef<'_>,
    _event: crate::time::TimerEvent,
) {
    #[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
    TEST_RETRY_CPU.store(
        crate::cpu::current_index().map_or(usize::MAX, |cpu| cpu.get()),
        Ordering::Release,
    );
    RETRY_DUE.store(true, Ordering::Release);
    request();
}
