// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Stable VM-owned vCPU notification endpoints.
//!
//! Scheduler Thread state is the sole authority for execution location. This
//! object retains only vCPU identity, its immutable Thread binding, and saved
//! interrupt-model work which must survive descheduling and migration.

use hyper::sync::InterruptSpinLock;
use hyper::sync::PublishedOnce;

use super::reconcile::ReconcilePublication;
use crate::task::thread::ThreadId;
use {crate::object::SignalMask, crate::object::SignalState};

pub(super) struct VcpuEndpoint {
    // Drop the timer reservation before every field reachable by its raw
    // callback context. An armed reservation fail-stops here, before endpoint
    // condition state could be destroyed underneath an in-flight callback.
    timer: crate::time::IrqCallout,
    id: u32,
    thread: PublishedOnce<ThreadId>,
    reconcile: ReconcilePublication,
    state: super::endpoint_state::EndpointState,
    signals: SignalState,
    mmio: InterruptSpinLock<hyper_vm_models::device::mmio::PendingMmio, crate::hal::irq::LocalMask>,
    wait: InterruptSpinLock<WaitState, crate::hal::irq::LocalMask>,
    waiters: crate::task::WaitQueue,
}

struct WaitState {
    publication: super::endpoint_wait::WaitPublication<crate::task::WaitTicket>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct WaitTicket {
    epoch: u64,
}

pub(super) enum PreparedWait<'a> {
    Changed,
    Park(EndpointPark<'a>),
    Completed(crate::task::WaitOutcome),
}

pub(super) struct EndpointPark<'a> {
    endpoint: &'a VcpuEndpoint,
    waiter: crate::task::WaitTicket,
    park: crate::task::scheduler::ParkToken,
}

impl EndpointPark<'_> {
    pub(super) fn complete(self) -> crate::task::WaitOutcome {
        let outcome = crate::task::scheduler::complete_park(self.park);
        self.endpoint.complete_wait(self.waiter, outcome);
        outcome
    }
}

enum LockedWait {
    Changed,
    Park {
        commit: crate::task::scheduler::ParkCommit,
        waiter: crate::task::WaitTicket,
    },
    Completed(crate::task::WaitOutcome),
}

impl VcpuEndpoint {
    pub(super) fn try_new(id: u32) -> Result<Self, crate::time::Error> {
        Ok(Self {
            timer: crate::time::IrqCallout::try_new()?,
            id,
            thread: PublishedOnce::new(),
            reconcile: ReconcilePublication::new(),
            state: super::endpoint_state::EndpointState::unbound(),
            signals: SignalState::new(),
            mmio: InterruptSpinLock::new(hyper_vm_models::device::mmio::PendingMmio::new()),
            wait: InterruptSpinLock::new(WaitState {
                publication: super::endpoint_wait::WaitPublication::new(),
            }),
            waiters: crate::task::WaitQueue::new(),
        })
    }

    pub(super) fn bind_thread(&self, thread: ThreadId) -> Result<(), ()> {
        if thread == ThreadId::BOOTSTRAP {
            return Err(());
        }
        self.thread.publish(thread).map_err(|_| ())?;
        self.state.publish_dormant().map_err(|_| ())?;
        Ok(())
    }

    /// Publishes runnable lifecycle before the scheduler can enter the guest.
    pub(super) fn start(&self) -> Result<(), super::endpoint_state::TransitionError> {
        let Some(thread) = self.thread() else {
            return Err(super::endpoint_state::TransitionError::Corrupt);
        };
        self.state.publish_started()?;
        match crate::task::scheduler::thread_ready(thread) {
            Ok(true) => Ok(()),
            Ok(false) | Err(_) => crate::diagnostics::crash::fatal(format_args!(
                "HypeR: committed vCPU readiness could not be published"
            )),
        }
    }

    pub(super) fn thread(&self) -> Option<ThreadId> {
        self.thread.get().copied()
    }

    #[allow(
        dead_code,
        reason = "selected guest backends opt into deferred userspace MMIO"
    )]
    pub(super) fn stage_mmio(
        &self,
        device: u64,
        access: hyper_vm_models::exit::MmioAccess,
    ) -> Result<(), hyper_vm_models::device::mmio::Error> {
        self.mmio.with(|state| state.stage(device, access))
    }

    pub(super) fn publish_mmio(&self) -> Result<(), hyper_vm_models::device::mmio::Error> {
        self.mmio.with(|state| {
            state.publish()?;
            self.update_mmio_signal(true);
            Ok(())
        })
    }

    pub(super) fn pending_mmio(&self) -> Option<hyper_vm_models::device::mmio::Request> {
        self.mmio.with(|state| state.pending())
    }

    pub(super) fn complete_mmio(
        &self,
        id: u64,
        action: hyper_vm_models::exit::MmioAction,
    ) -> Result<(), hyper_vm_models::device::mmio::Error> {
        self.mmio.with(|state| {
            state.complete(id, action)?;
            self.update_mmio_signal(false);
            Ok(())
        })?;
        self.signal_waiter();
        Ok(())
    }

    pub(super) fn take_mmio_completion(
        &self,
    ) -> Result<Option<hyper_vm_models::exit::MmioAction>, hyper_vm_models::device::mmio::Error>
    {
        self.mmio.with(|state| state.take_completed())
    }

    fn cancel_mmio(&self) {
        self.mmio.with(|state| {
            state.close();
            self.update_mmio_signal(false);
        });
    }

    fn update_mmio_signal(&self, pending: bool) {
        let signal = SignalMask::from_trusted_bits(
            hyper::abi::native::HYPER_NATIVE_SIGNAL_VIRTUAL_CPU_MMIO_REQUEST,
        );
        let (clear, set) = if pending {
            (SignalMask::EMPTY, signal)
        } else {
            (signal, SignalMask::EMPTY)
        };
        if self.signals.update(clear, set).is_err() {
            hyper::debug::invariant_failure("vm::endpoint::update_mmio_signal invariant");
        }
    }

    pub(super) fn publish_reconcile(&self) -> Result<(), super::endpoint_state::StateError> {
        // Close and publication use separate atomics. A producer which already
        // observed Open may publish dirty after close; that work is inert and
        // memory-safe because the installed VM/controller remain strongly
        // owned. Future removal must stop and drain backend producers before
        // reclaiming endpoint storage; close alone is not quiescence.
        self.state.ensure_live()?;
        self.reconcile.publish();
        Ok(())
    }

    pub(super) fn signal_waiter(&self) {
        self.wait.with(|state| {
            let ticket = match state.publication.signal() {
                Ok(ticket) => ticket,
                Err(super::endpoint_wait::Error::EpochExhausted) => {
                    crate::diagnostics::crash::fatal(format_args!(
                        "HypeR: vCPU wait epoch exhausted"
                    ))
                }
                Err(super::endpoint_wait::Error::WaiterAlreadyRegistered) => {
                    crate::diagnostics::crash::fatal(format_args!(
                        "HypeR: invalid endpoint signal state"
                    ))
                }
            };
            let Some(ticket) = ticket else {
                return;
            };
            let resolved = match crate::task::scheduler::notify_registered_fair_boundary(ticket) {
                Ok(resolved) => resolved,
                Err(error) => crate::diagnostics::crash::fatal(format_args!(
                    "HypeR: vCPU endpoint could not resolve its exact wait: {error:?}"
                )),
            };
            // Cancellation may have won scheduler arbitration after the
            // endpoint ticket was registered. The signal still owns and
            // consumes that exact endpoint ticket; `EndpointPark::complete`
            // performs the matching resumed-side cleanup.
            let _notification_won = resolved.won;
        })
    }

    pub(super) fn wait_ticket(&self) -> WaitTicket {
        self.wait.with(|state| WaitTicket {
            epoch: state.publication.snapshot(),
        })
    }

    pub(super) fn arm_timer(
        &self,
        deadline: u64,
    ) -> Result<crate::time::ArmedIrqCallout<'_>, crate::time::Error> {
        self.timer.arm(
            deadline,
            notify_timer_waiter,
            core::ptr::from_ref(self).expose_provenance(),
        )
    }

    pub(super) fn prepare_wait(
        &self,
        ticket: WaitTicket,
    ) -> Result<PreparedWait<'_>, crate::task::scheduler::Error> {
        use {crate::task::WaitMobility, crate::task::scheduler};

        let registration = scheduler::begin_wait(WaitMobility::Migratable)?;
        // SAFETY: the retained local-mask state is either dropped on this CPU
        // or transferred immediately into the committed scheduler handoff.
        let (prepared, interrupt_mask) = unsafe {
            self.wait.with_mask_retained(|state| {
                // Stop is durable, unlike an interrupt notification. A runner
                // can snapshot a fresh epoch after the stop signal and would
                // otherwise park again. Under this lock either stop already
                // closed admission, or its subsequent signal resolves the
                // registration before request_stop returns to the scheduler.
                let admission = match self.state.ensure_live() {
                    Ok(()) => state
                        .publication
                        .register(ticket.epoch, registration.ticket()),
                    Err(super::endpoint_state::StateError::Closed(_)) => {
                        Ok(super::endpoint_wait::Registration::Changed)
                    }
                    Err(super::endpoint_state::StateError::Corrupt) => {
                        hyper::debug::invariant_failure("vCPU wait lifecycle corrupt")
                    }
                };
                match admission {
                    Ok(super::endpoint_wait::Registration::Changed) => {
                        match scheduler::finish_wait(registration)? {
                            None => Ok(LockedWait::Changed),
                            Some(_) => Err(scheduler::Error::InvalidWaitRegistration),
                        }
                    }
                    Ok(super::endpoint_wait::Registration::Registered) => {
                        let waiter = registration.ticket();
                        match scheduler::prepare_registered_park_locked(&self.waiters, registration)
                        {
                            Ok(scheduler::PrepareWait::Park(commit)) => {
                                Ok(LockedWait::Park { commit, waiter })
                            }
                            Ok(scheduler::PrepareWait::Completed(outcome)) => {
                                complete_publication(state, waiter, outcome);
                                Ok(LockedWait::Completed(outcome))
                            }
                            Err(error) => {
                                complete_publication(
                                    state,
                                    waiter,
                                    crate::task::WaitOutcome::Cancelled,
                                );
                                Err(error)
                            }
                        }
                    }
                    Err(_) => {
                        crate::diagnostics::crash::fatal(format_args!(
                            "HypeR: vCPU endpoint published two wait registrations"
                        ));
                    }
                }
            })
        };
        match prepared? {
            LockedWait::Park { commit, waiter } => Ok(PreparedWait::Park(EndpointPark {
                endpoint: self,
                waiter,
                park: scheduler::retain_park_mask(commit, interrupt_mask),
            })),
            LockedWait::Changed => {
                drop(interrupt_mask);
                Ok(PreparedWait::Changed)
            }
            LockedWait::Completed(outcome) => {
                drop(interrupt_mask);
                Ok(PreparedWait::Completed(outcome))
            }
        }
    }

    fn complete_wait(&self, waiter: crate::task::WaitTicket, outcome: crate::task::WaitOutcome) {
        self.wait
            .with(|state| complete_publication(state, waiter, outcome));
    }

    pub(super) fn claim_detached_reconcile(&self) -> bool {
        self.reconcile.claim_before_admission()
    }

    pub(super) fn restore_reconcile(&self) {
        self.reconcile.restore();
    }

    pub(super) fn reconcile_pending(&self) -> bool {
        self.reconcile.pending()
    }

    pub(super) fn is_valid_for(&self, id: u32) -> bool {
        self.id == id
    }

    pub(super) fn close(
        &self,
        expected_thread: ThreadId,
        reason: super::endpoint_state::TerminalReason,
    ) -> Result<super::endpoint_state::GuestCloseOutcome, super::endpoint_state::TransitionError>
    {
        if self.thread() != Some(expected_thread) {
            return Err(super::endpoint_state::TransitionError::Corrupt);
        }
        self.state.close_guest(reason)
    }

    pub(super) fn request_stop(
        &self,
        expected_thread: ThreadId,
        reason: super::endpoint_state::AdministrativeStopReason,
    ) -> Result<super::endpoint_state::StopRequestOutcome, super::endpoint_state::StateError> {
        if self.thread() != Some(expected_thread) {
            return Err(super::endpoint_state::StateError::Corrupt);
        }
        let outcome = self.state.request_stop(reason)?;
        if outcome == super::endpoint_state::StopRequestOutcome::Published {
            self.cancel_mmio();
            // Stop and interrupt delivery share the exact endpoint wait ticket.
            // A suspended WFI continuation is made runnable so it can unwind
            // its EndpointPark and ArmedIrqCallout ownership normally.
            self.signal_waiter();
        }
        Ok(outcome)
    }

    pub(super) fn stop_requested(
        &self,
        expected_thread: ThreadId,
    ) -> Result<
        Option<super::endpoint_state::AdministrativeStopReason>,
        super::endpoint_state::StateError,
    > {
        if self.thread() != Some(expected_thread) {
            return Err(super::endpoint_state::StateError::Corrupt);
        }
        match self.state.lifecycle()? {
            super::endpoint_state::Lifecycle::StopRequested(reason) => Ok(Some(reason)),
            _ => Ok(None),
        }
    }

    pub(super) fn publish_hardware_detached(
        &self,
        expected_thread: ThreadId,
        reason: super::endpoint_state::AdministrativeStopReason,
    ) -> Result<(), super::endpoint_state::TransitionError> {
        if self.thread() != Some(expected_thread) {
            return Err(super::endpoint_state::TransitionError::Corrupt);
        }
        self.state.publish_hardware_detached(reason)
    }

    pub(super) fn publish_reaped(
        &self,
        expected_thread: ThreadId,
        reason: super::endpoint_state::ClosureReason,
    ) -> Result<(), super::endpoint_state::TransitionError> {
        if self.thread() != Some(expected_thread) {
            return Err(super::endpoint_state::TransitionError::Corrupt);
        }
        self.state.publish_reaped(reason)?;
        let terminated = SignalMask::from_trusted_bits(
            hyper::abi::native::HYPER_NATIVE_SIGNAL_VIRTUAL_CPU_TERMINATED,
        );
        if self.signals.update(SignalMask::EMPTY, terminated).is_err() {
            hyper::debug::invariant_failure("vm::endpoint::publish_reaped invariant");
        }
        Ok(())
    }

    pub(super) const fn signals(&self) -> &SignalState {
        &self.signals
    }

    pub(super) fn lifecycle(
        &self,
    ) -> Result<super::endpoint_state::Lifecycle, super::endpoint_state::StateError> {
        self.state.lifecycle()
    }

    pub(super) fn thread_absence_is_terminal(
        &self,
    ) -> Result<bool, super::endpoint_state::StateError> {
        self.state.thread_absence_is_terminal()
    }
}

fn notify_timer_waiter(context: usize) {
    let pointer = core::ptr::with_exposed_provenance::<VcpuEndpoint>(context);
    // SAFETY: the armed timer borrows this stable installed endpoint until its
    // exact callback completes or cancellation returns its reserved node.
    let endpoint = unsafe { &*pointer };
    endpoint.signal_waiter();
}

fn complete_publication(
    state: &mut WaitState,
    waiter: crate::task::WaitTicket,
    outcome: crate::task::WaitOutcome,
) {
    if let Err(error) = state
        .publication
        .complete(waiter, outcome == crate::task::WaitOutcome::Notified)
    {
        crate::diagnostics::crash::fatal(format_args!(
            "HypeR: vCPU endpoint wait completion mismatch: {error:?}"
        ));
    }
}

#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
mod self_test {
    use hyper::cpu::CpuIndex;
    use hyper::sync::PublishedOnce;
    use hyper::sync::atomic::{AtomicUsize, Ordering};

    use super::{PreparedWait, VcpuEndpoint};
    use crate::sync::Semaphore;
    use crate::task::WaitOutcome;
    use {crate::task::scheduler, crate::task::scheduler::CpuMask};

    static ENDPOINT: PublishedOnce<VcpuEndpoint> = PublishedOnce::new();
    static PHASE: AtomicUsize = AtomicUsize::new(0);
    static OUTCOMES: AtomicUsize = AtomicUsize::new(0);
    static DONE: Semaphore = Semaphore::new(0);

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub(crate) enum Error {
        Allocation,
        Scheduler(scheduler::Error),
        Sleep(crate::task::SleepError),
        State(usize),
    }

    impl From<scheduler::Error> for Error {
        fn from(error: scheduler::Error) -> Self {
            Self::Scheduler(error)
        }
    }

    impl From<crate::task::SleepError> for Error {
        fn from(error: crate::task::SleepError) -> Self {
            Self::Sleep(error)
        }
    }

    pub(crate) fn run() -> Result<(), Error> {
        let prepared_endpoint = VcpuEndpoint::try_new(0).map_err(|_| Error::Allocation)?;
        ENDPOINT
            .publish(prepared_endpoint)
            .map_err(|_| Error::State(1))?;
        PHASE.store(0, Ordering::Release);
        OUTCOMES.store(0, Ordering::Release);

        let worker = scheduler::kthread_create_with_affinity(
            "wait/vcpu-endpoint",
            worker,
            0,
            CpuMask::single(CpuIndex::BOOT),
        )?;
        endpoint()?
            .bind_thread(worker)
            .map_err(|_| Error::State(7))?;
        endpoint()?.start().map_err(|_| Error::State(8))?;

        wait_for_phase(1)?;
        endpoint()?.signal_waiter();
        wait_for_phase(2)?;
        if !scheduler::cancel_waiter(&endpoint()?.waiters, worker)? {
            return Err(Error::State(2));
        }
        // The endpoint still owns its exact ticket. This later signal must be
        // a benign scheduler loser and must let resumed cleanup consume the
        // already-cleared endpoint publication.
        endpoint()?.signal_waiter();
        wait_for_phase(3)?;
        endpoint()?
            .request_stop(
                worker,
                crate::vm::endpoint_state::AdministrativeStopReason::Requested,
            )
            .map_err(|_| Error::State(9))?;
        wait_for_done()?;
        let outcomes = OUTCOMES.load(Ordering::Acquire);
        if outcomes != 0b1111 {
            return Err(Error::State(outcomes));
        }
        Ok(())
    }

    fn endpoint() -> Result<&'static VcpuEndpoint, Error> {
        ENDPOINT.get().ok_or(Error::State(4))
    }

    fn wait_for_phase(expected: usize) -> Result<(), Error> {
        if crate::task::wait_for_test_progress(crate::task::TEST_PROGRESS_TIMEOUT_NS, || {
            Ok::<_, Error>(PHASE.load(Ordering::Acquire) == expected)
        })? {
            Ok(())
        } else {
            Err(Error::State(5))
        }
    }

    fn wait_for_done() -> Result<(), Error> {
        if crate::task::wait_for_test_progress(crate::task::TEST_PROGRESS_TIMEOUT_NS, || {
            Ok::<_, Error>(DONE.try_acquire())
        })? {
            Ok(())
        } else {
            Err(Error::State(6))
        }
    }

    extern "C" fn worker(_argument: usize) {
        for (round, expected) in [
            WaitOutcome::Notified,
            WaitOutcome::Cancelled,
            WaitOutcome::Notified,
        ]
        .into_iter()
        .enumerate()
        {
            let outcome = match wait_once(round + 1) {
                Ok(outcome) => outcome,
                Err(code) => {
                    OUTCOMES.store(0x100 + code, Ordering::Release);
                    let _ = DONE.release();
                    return;
                }
            };
            if outcome != expected {
                OUTCOMES.store(0x200 + round, Ordering::Release);
                let _ = DONE.release();
                return;
            }
            OUTCOMES.fetch_or(1 << round, Ordering::AcqRel);
        }
        if let Err(code) = verify_stop_before_park() {
            OUTCOMES.store(0x400 + code, Ordering::Release);
            let _ = DONE.release();
            return;
        }
        OUTCOMES.fetch_or(1 << 3, Ordering::AcqRel);
        if DONE.release().is_err() {
            OUTCOMES.store(0x300, Ordering::Release);
        }
    }

    /// A stop is permanent even when its notification precedes a new snapshot.
    /// Exercise the production scheduler park, and unwind it on regression so
    /// the test reports a failure instead of leaving a suspended continuation.
    fn verify_stop_before_park() -> Result<(), usize> {
        let endpoint = VcpuEndpoint::try_new(0).map_err(|_| 1usize)?;
        let thread = scheduler::current_thread_id().map_err(|_| 2usize)?;
        endpoint.bind_thread(thread).map_err(|_| 3usize)?;
        endpoint.state.publish_started().map_err(|_| 4usize)?;
        let old_ticket = endpoint.wait_ticket();
        endpoint
            .request_stop(
                thread,
                crate::vm::endpoint_state::AdministrativeStopReason::Requested,
            )
            .map_err(|_| 5usize)?;
        for ticket in [old_ticket, endpoint.wait_ticket()] {
            match endpoint.prepare_wait(ticket).map_err(|_| 6usize)? {
                PreparedWait::Changed => {}
                PreparedWait::Completed(_) => return Err(7),
                PreparedWait::Park(park) => {
                    endpoint.signal_waiter();
                    let _ = park.complete();
                    return Err(8);
                }
            }
        }
        Ok(())
    }

    fn wait_once(phase: usize) -> Result<WaitOutcome, usize> {
        let endpoint = endpoint().map_err(|_| 1usize)?;
        let ticket = endpoint.wait_ticket();
        match endpoint.prepare_wait(ticket).map_err(|_| 2usize)? {
            PreparedWait::Changed => Err(3),
            PreparedWait::Completed(outcome) => Ok(outcome),
            PreparedWait::Park(park) => {
                PHASE.store(phase, Ordering::Release);
                Ok(park.complete())
            }
        }
    }
}

#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
pub(crate) use self_test::{Error as WaitSelfTestError, run as run_wait_self_test};
