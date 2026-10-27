// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! One sleeping owner of the command/event queues; IRQs only prompt work.

use super::{Controller, Error, notifications};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use {crate::irq::interrupt, crate::sync::Completion, crate::sync::Mutex, crate::task::scheduler};
use {
    hyper::hal::interrupt::InterruptId, hyper::hal::interrupt::InterruptPriority,
    hyper::hal::interrupt::InterruptTrigger, hyper::mm::FallibleArc, hyper::sync::DeferredWork,
    hyper::sync::InterruptSpinLock, hyper::sync::WorkDisposition, hyper_drivers::iommu::smmuv3,
    hyper_platform::PlatformInterrupt,
};

static OWNER: InterruptSpinLock<Option<FallibleArc<Runtime>>, crate::hal::irq::LocalMask> =
    InterruptSpinLock::new(None);
static WORK: DeferredWork = DeferredWork::new();
static WAKE: Completion = Completion::new();
static WORKER_READY: AtomicBool = AtomicBool::new(false);
static IRQ_COUNT: AtomicUsize = AtomicUsize::new(0);
#[cfg(all(feature = "kernel-test", feature = "kernel-smmuv3-test"))]
static FAILURE_REPORTED: AtomicBool = AtomicBool::new(false);
const BATCH: usize = 32;

pub(crate) struct Runtime {
    controller: Mutex<Controller>,
    observers: notifications::Observers,
    registrations: [Option<interrupt::Registration>; 2],
}

impl Runtime {
    pub(crate) fn observe(
        &self,
        stream: u32,
        events: &FallibleArc<crate::device::assigned::events::Events>,
    ) -> Result<(), smmuv3::Error> {
        self.observers.register(stream, events)?;
        // A controller may have failed before this observer registered. Register
        // first, then inspect: a racing worker either reports it or this snapshot
        // does. The device retains only the first report, so both are harmless.
        if let Some(failure) = self.with_controller(|controller| controller.failure()) {
            let mut record = notifications::controller_record(failure);
            record.stream_id = stream;
            events.publish(record);
        }
        Ok(())
    }

    /// Queue access and failure policy share one boundary. A command timeout
    /// need not raise an IRQ, so the caller must not leave a failed controller
    /// unnoticed behind a sleeping fault worker.
    pub(crate) fn with_controller<R>(&self, operation: impl FnOnce(&mut Controller) -> R) -> R {
        let (result, failure) = {
            let mut controller = lock(self);
            let result = operation(&mut controller);
            (result, controller.failure())
        };
        if let Some(failure) = failure {
            if matches!(failure.containment, smmuv3::Containment::Unconfirmed(_)) {
                crate::diagnostics::crash::fatal(format_args!(
                    "HypeR: SMMUv3 could not confirm DMA containment: {failure:?}"
                ));
            }
            let _ = WORK.request();
            // Thread context is already outside the controller/IRQ locks.
            // Consume our durable prompt now; no later hardware IRQ is needed.
            service_irq_prompt();
        }
        result
    }
}

pub(super) fn install(
    mut controller: Controller,
    routes: (PlatformInterrupt, Option<PlatformInterrupt>),
    root: interrupt::IrqDomainId,
) -> Result<FallibleArc<Runtime>, Error> {
    let registrations = match register(routes, root) {
        Ok(registrations) => registrations,
        Err(error) => {
            controller.fail_closed(smmuv3::Error::Failed);
            return Err(error);
        }
    };
    let runtime = Runtime {
        controller: Mutex::new(controller),
        observers: notifications::Observers::new(),
        registrations,
    };
    let runtime = match FallibleArc::try_new_or_return(runtime) {
        Ok(runtime) => runtime,
        Err((_, runtime)) => {
            rollback(runtime);
            return Err(Error::Driver(smmuv3::Error::Allocation));
        }
    };
    let thread = match scheduler::kthread_create("ksmmu-fault", worker, 0) {
        Ok(thread) => thread,
        Err(error) => {
            match runtime.try_unwrap() {
                Ok(runtime) => rollback(runtime),
                Err(_) => hyper::debug::invariant_failure("unpublished SMMU owner shared"),
            }
            return Err(Error::Scheduler(error));
        }
    };
    // Initialization has one caller. From publication onward both IRQ context
    // and the worker have a permanent owner; there is no hot removal or reset.
    OWNER.with(|owner| {
        if owner.is_some() {
            hyper::debug::invariant_failure("SMMU owner already installed");
        }
        *owner = Some(runtime.clone());
    });
    if !WORK.claim_initial_worker() {
        hyper::debug::invariant_failure("SMMU worker already installed");
    }
    WORKER_READY.store(true, Ordering::Release);
    match scheduler::thread_ready(thread) {
        Ok(true) => Ok(runtime),
        result => crate::diagnostics::crash::fatal(format_args!(
            "HypeR: SMMU worker activation failed: {result:?}"
        )),
    }
}

fn register(
    routes: (PlatformInterrupt, Option<PlatformInterrupt>),
    root: interrupt::IrqDomainId,
) -> Result<[Option<interrupt::Registration>; 2], Error> {
    fn one(
        root: interrupt::IrqDomainId,
        route: PlatformInterrupt,
    ) -> Result<interrupt::Registration, Error> {
        let trigger = match route.trigger {
            hyper_platform::PlatformInterruptTrigger::Edge => InterruptTrigger::Edge,
            hyper_platform::PlatformInterruptTrigger::Level => InterruptTrigger::Level,
        };
        let prepared = root
            .prepare_shared_mapping(
                InterruptId::new(route.interrupt),
                InterruptPriority::Normal,
                trigger,
                0,
                handler,
            )
            .map_err(Error::Interrupt)?;
        match interrupt::activate(prepared) {
            Ok(registration) => Ok(registration),
            Err(failure) => {
                let (error, prepared) = failure.into_parts();
                if let Err(failure) = interrupt::discard_prepared(prepared) {
                    let (error, _prepared) = failure.into_parts();
                    crate::diagnostics::crash::fatal(format_args!(
                        "HypeR: SMMU IRQ rollback failed: {error:?}"
                    ));
                }
                Err(Error::Interrupt(error))
            }
        }
    }
    let first = one(root, routes.0)?;
    let Some(second) = routes.1 else {
        return Ok([Some(first), None]);
    };
    match one(root, second) {
        Ok(second) => Ok([Some(first), Some(second)]),
        Err(error) => {
            unregister(first);
            Err(error)
        }
    }
}

fn unregister(registration: interrupt::Registration) {
    if let Err(failure) = interrupt::unregister(registration) {
        let (error, _registration) = failure.into_parts();
        crate::diagnostics::crash::fatal(format_args!("HypeR: SMMU IRQ removal failed: {error:?}"));
    }
}

fn rollback(mut runtime: Runtime) {
    runtime
        .controller
        .get_mut()
        .fail_closed(smmuv3::Error::Failed);
    for registration in runtime.registrations.into_iter().flatten() {
        unregister(registration);
    }
}

fn handler(
    _: interrupt::VirtualInterrupt,
    _: usize,
    _frame: &crate::hal::irq::InterruptFrame<'_>,
) -> interrupt::HandlerResult {
    IRQ_COUNT.fetch_add(1, Ordering::Relaxed);
    let _ = WORK.request(); // This entry's IRQ tail services the durable prompt.
    interrupt::HandlerResult::HandledAndMaskLocal
}

pub(crate) fn service_irq_prompt() {
    // A stale/spurious edge can arrive while the two routes are installed.
    // Keep its prompt sticky until initial worker ownership is published; an
    // early wake must not steal the initial worker's notification ownership.
    if WORKER_READY.load(Ordering::Acquire)
        && WORK.consume_prompt()
        && WORK.claim_notification()
        && let Err(error) = WAKE.complete()
    {
        crate::diagnostics::crash::fatal(format_args!("HypeR: SMMU fault wake failed: {error:?}"));
    }
}

pub(crate) fn owner() -> Option<FallibleArc<Runtime>> {
    OWNER.with(|owner| owner.clone())
}

#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
pub(crate) fn permanent_worker_count_for_test() -> usize {
    usize::from(owner().is_some())
}

#[cfg(all(feature = "kernel-test", feature = "kernel-smmuv3-test"))]
pub(crate) fn interrupt_count() -> usize {
    IRQ_COUNT.load(Ordering::Relaxed)
}

#[cfg(all(feature = "kernel-test", feature = "kernel-smmuv3-test"))]
pub(crate) fn failure_reported() -> bool {
    FAILURE_REPORTED.load(Ordering::Acquire)
}

extern "C" fn worker(_: usize) {
    let Some(runtime) = owner() else {
        hyper::debug::invariant_failure("SMMU worker owner missing");
    };
    runtime.with_controller(|controller| {
        let _ = controller.enable_interrupts();
    });
    loop {
        WORK.begin_batch();
        let more = service(&runtime);
        match WORK.finish_batch(more) {
            WorkDisposition::Continue => {
                if let Err(error) = scheduler::cond_resched() {
                    worker_failed(error);
                }
            }
            WorkDisposition::Wait => {
                if let Err(error) = WAKE.wait() {
                    crate::diagnostics::crash::fatal(format_args!(
                        "HypeR: SMMU worker wait failed: {error:?}"
                    ));
                }
            }
        }
    }
}

fn lock(runtime: &Runtime) -> crate::sync::MutexGuard<'_, Controller> {
    runtime.controller.lock().unwrap_or_else(|error| {
        crate::diagnostics::crash::fatal(format_args!("HypeR: SMMU owner lock failed: {error:?}"))
    })
}

fn worker_failed(error: scheduler::Error) -> ! {
    crate::diagnostics::crash::fatal(format_args!(
        "HypeR: SMMU worker scheduling failed: {error:?}"
    ))
}

fn service(runtime: &Runtime) -> bool {
    let mut reports = [None; BATCH];
    let mut count = 0;
    let failure = runtime.with_controller(|controller| {
        for report in &mut reports {
            match controller.next_event() {
                Ok(Some(event)) => {
                    count += 1;
                    match controller.quarantine_fault(event) {
                        Ok(smmuv3::FaultOutcome::Quarantined { domain }) => {
                            *report = Some((event, domain))
                        }
                        Ok(smmuv3::FaultOutcome::AlreadyQuarantined) => {}
                        Err(_) => break,
                    }
                }
                Ok(None) | Err(_) => break,
            }
        }
        controller.failure()
    });
    for (event, domain) in reports.into_iter().flatten() {
        runtime
            .observers
            .publish(Some(event.stream()), notifications::stream_record(event));
        crate::pr_err!(
            "HypeR: SMMUv3 quarantined SID {} domain {:?}: {:?}, address {:#x}, read={}",
            event.stream(),
            domain,
            event.fault(),
            event.address(),
            event.read()
        );
    }
    if let Some(failure) = failure {
        runtime
            .observers
            .publish(None, notifications::controller_record(failure));
        crate::pr_err!(
            "HypeR: SMMUv3 controller quarantined: {failure:?}; command error {:?}; backing retained",
            failure.command_error()
        );
        #[cfg(all(feature = "kernel-test", feature = "kernel-smmuv3-test"))]
        FAILURE_REPORTED.store(true, Ordering::Release);
        // Hardware IRQs disabled and all backing retained. A failed controller
        // cannot automatically resume; leave both controller routes masked.
        loop {
            if let Err(error) = WAKE.wait() {
                crate::diagnostics::crash::fatal(format_args!(
                    "HypeR: quarantined SMMU worker wait failed: {error:?}"
                ));
            }
        }
    }
    // No device/queue mutex is held while acquiring the IRQ registry lock.
    for registration in runtime.registrations.iter().flatten() {
        if let Err(error) = interrupt::enable_registered_shared(registration) {
            runtime.with_controller(|controller| controller.fail_closed(smmuv3::Error::Failed));
            crate::diagnostics::crash::fatal(format_args!(
                "HypeR: SMMU IRQ rearm failed: {error:?}"
            ));
        }
    }
    count == BATCH
}
