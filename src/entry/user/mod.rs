// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Native Thread execution and sleepable synchronous-call policy.

mod fault;
#[cfg(all(
    CONFIG_ARCH_AARCH64,
    all(feature = "kernel-test", feature = "kernel-self-test")
))]
pub(crate) mod self_test;

use core::cell::UnsafeCell;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicPtr, Ordering};

use hyper::abi::native::NativeInvocation;
use hyper::cpu::PerCpu;
use hyper::hal::user::{
    NativeCallAction, NativeCallHandler, NativeCallService, UserFault, UserFaultKind,
};
use hyper::sync::InterruptMaskGuard;

use super::services::ProcessServices;
use crate::abi::native;
use crate::task::scheduler;
use {crate::mm::user_space::ActiveNativeAddressSpace, crate::mm::user_space::UserAddress};
use {
    crate::process::Process, crate::process::RunAdmissionError, crate::process::TerminalReason,
    crate::process::UserExecution, crate::process::UserThread,
};

/// Stable ownership established once for the scheduler Thread's entire entry.
struct UserSession {
    thread: UserThread,
    process: Process,
}

impl UserSession {
    fn attach(current: scheduler::CurrentUser) -> (Self, NonNull<UserExecution>) {
        validate_current_stack(current.stack);
        let execution = current.execution;
        let thread = current.object;
        if thread.scheduler_id() != Some(current.thread) {
            crate::diagnostics::crash::fatal(format_args!(
                "HypeR: native-user scheduler identity is inconsistent"
            ));
        }
        // SAFETY: The caller pins the current Thread through this attachment.
        // Its scheduler-owned payload remains allocated until entry returns,
        // including while syscall continuations block or migrate.
        let process = unsafe { execution.as_ref() }.process().clone();
        if process.id() != thread.process_id() {
            crate::diagnostics::crash::fatal(format_args!(
                "HypeR: native-user Process ownership is inconsistent"
            ));
        }
        (Self { thread, process }, execution)
    }

    fn stop_requested(&self) -> bool {
        if self.thread.stop_requested() {
            return true;
        }
        if let Some(reason) = self.process.requested_stop_reason() {
            // Preserve the Process reason even if this continuation observes
            // closed admission before the scheduler's per-Thread stop fan-out.
            self.thread.request_stop(reason);
            return true;
        }
        false
    }
}

/// A Thread-owned continuation with optional current-CPU translation ownership.
///
/// The cell is accessed only by this Thread with IRQs masked, or by its masked
/// switch hook before the saved continuation can be published on another CPU.
/// The hook empties it before migration; a suspended syscall owns no CPU token.
struct NativeMachineSession<'owner> {
    session: &'owner UserSession,
    execution: &'owner UserExecution,
    active: UnsafeCell<Option<ActiveNativeAddressSpace<'owner>>>,
}

static ACTIVE_SESSIONS: PerCpu<AtomicPtr<NativeMachineSession<'static>>> =
    PerCpu::new([const { AtomicPtr::new(core::ptr::null_mut()) }; hyper::cpu::MAX_CPUS]);

impl NativeMachineSession<'_> {
    /// Observes deferred scheduling and stop before admitting an EL0 return.
    /// Enters with IRQs enabled and returns with IRQs masked, including stop.
    fn prepare_user_return(&self) -> bool {
        loop {
            if let Err(error) = scheduler::ensure_sleepable() {
                fail_run("native-user return left sleepable context", error);
            }
            crate::hal::irq::mask_local();
            if self.session.stop_requested() {
                return false;
            }
            if preemption_pending() {
                crate::hal::irq::enable_local();
                if let Err(error) = scheduler::cond_resched() {
                    fail_run("failed to schedule native-user continuation", error);
                }
                continue;
            }
            // SAFETY: Masking excludes IRQ-tail suspension. Only this current
            // continuation and that suspension hook access the optional token.
            if unsafe { &*self.active.get() }.is_some() {
                return true;
            }
            crate::hal::irq::enable_local();
            let identifier = match self.execution.address_space().acquire_identifier() {
                Ok(identifier) => identifier,
                Err(error) if error.is_identifier_busy() => {
                    if let Err(error) = scheduler::yield_now() {
                        fail_run("failed to yield under Native translation pressure", error);
                    }
                    continue;
                }
                Err(error) => fail_run("failed to acquire Native translation identifier", error),
            };
            crate::hal::irq::mask_local();
            if self.session.stop_requested() {
                drop(identifier);
                return false;
            }
            let activated = {
                // SAFETY: This lexical guard is created while already masked
                // and ends before IRQs can be enabled or the Thread can move.
                let mask = unsafe { InterruptMaskGuard::<crate::hal::irq::LocalMask>::acquire() };
                let access = match crate::hal::user::prepare_kernel_access(&mask) {
                    Ok(access) => access,
                    Err(error) => {
                        fail_run("failed to establish native-user access isolation", error)
                    }
                };
                // SAFETY: IRQs remain masked through storing this token and
                // registering the switch hook below. Every actual switch
                // consumes it before publishing this Thread's saved context.
                unsafe { self.execution.address_space().activate(&access, identifier) }
            };
            match activated {
                Ok(active) => {
                    // SAFETY: No activation exists and this masked continuation
                    // exclusively owns the Thread's empty residency slot.
                    unsafe { *self.active.get() = Some(active) };
                    let cpu = current_cpu();
                    let pointer = core::ptr::from_ref(self)
                        .cast::<NativeMachineSession<'static>>()
                        .cast_mut();
                    if ACTIVE_SESSIONS[cpu]
                        .compare_exchange(
                            core::ptr::null_mut(),
                            pointer,
                            Ordering::Release,
                            Ordering::Relaxed,
                        )
                        .is_err()
                    {
                        hyper::debug::invariant_failure("Native CPU session already registered");
                    }
                    if self.session.stop_requested() {
                        return false;
                    }
                    if !preemption_pending() {
                        return true;
                    }
                    crate::hal::irq::enable_local();
                }
                Err(error) if error.is_mapping_conflict() => {
                    crate::hal::irq::enable_local();
                    if let Err(error) = scheduler::yield_now() {
                        fail_run("failed to yield under Native mapping contention", error);
                    }
                }
                Err(error) => fail_run("failed to activate native-user address space", error),
            }
        }
    }

    fn mapping_epoch(&self) -> Option<u64> {
        require_masked();
        // SAFETY: The current Thread owns this cell and IRQs exclude its hook.
        unsafe { &*self.active.get() }
            .as_ref()
            .map(ActiveNativeAddressSpace::mapping_epoch)
    }

    /// Leaves the current CPU after a final architecture exit.
    fn detach_address(&self) {
        require_masked();
        let pointer = ACTIVE_SESSIONS[current_cpu()].swap(core::ptr::null_mut(), Ordering::Acquire);
        if !pointer.is_null() && pointer.cast_const() != core::ptr::from_ref(self).cast() {
            hyper::debug::invariant_failure("Native exit belongs to a different CPU session");
        }
        // SAFETY: Architecture publication is closed and the masked current
        // Thread exclusively owns this cell. A previously suspended call has
        // neither a registered pointer nor an activation to consume.
        let active = unsafe { &mut *self.active.get() }.take();
        if active.is_some() != !pointer.is_null() {
            hyper::debug::invariant_failure("Native translation registration is inconsistent");
        }
        if let Some(active) = active
            && let Err(error) = active.leave()
        {
            fail_run("failed to leave native-user address space", error);
        }
    }
}

impl Drop for NativeMachineSession<'_> {
    fn drop(&mut self) {
        if self.active.get_mut().is_some() {
            hyper::debug::invariant_failure("Native session dropped with active translation");
        }
    }
}

// SAFETY: Dispatch enters ordinary IRQ-enabled Thread context without retaining
// a CPU-affine guard. The scheduler's single committed-switch seam withdraws
// architecture publication and empties this session's translation cell before
// any other Thread runs. The retained Thread stack owns the service and session
// throughout blocking/migration. Return masks IRQs, checks stop and scheduling,
// and reacquires translation when the continuation was actually switched out.
unsafe impl NativeCallHandler for NativeMachineSession<'_> {
    unsafe fn dispatch(&self, invocation: NativeInvocation) -> NativeCallAction {
        require_masked();
        crate::hal::irq::enable_local();
        if let Err(error) = scheduler::ensure_sleepable() {
            fail_run("native-user syscall did not enter sleepable context", error);
        }
        #[cfg(all(
            CONFIG_ARCH_AARCH64,
            all(feature = "kernel-test", feature = "kernel-self-test")
        ))]
        self_test::hold_syscall(&self.session.process, &self.session.thread, invocation);
        if self.session.stop_requested() {
            crate::hal::irq::mask_local();
            return NativeCallAction::Stop;
        }
        let services = ProcessServices::new(&self.session.process, &self.session.thread);
        let result = match native::dispatch(&services, invocation) {
            native::NativeAction::Return(result) => result,
            native::NativeAction::Yield(result) => {
                if let Err(error) = scheduler::yield_now() {
                    fail_run("failed to yield native-user Thread", error);
                }
                result
            }
            native::NativeAction::ExitThread { status } => {
                self.session
                    .thread
                    .request_stop(TerminalReason::ThreadExited { status });
                crate::hal::irq::mask_local();
                return NativeCallAction::Stop;
            }
            native::NativeAction::ExitProcess { status } => {
                self.session
                    .process
                    .request_stop(TerminalReason::ProcessExited { status });
                crate::hal::irq::mask_local();
                return NativeCallAction::Stop;
            }
        };
        if self.prepare_user_return() {
            NativeCallAction::Return(result)
        } else {
            NativeCallAction::Stop
        }
    }
}

/// Withdraws the outgoing Native owner at the one committed scheduler seam.
/// Scheduler `current` already names the incoming Thread here.
pub(crate) fn suspend_current() {
    require_masked();
    let pointer = ACTIVE_SESSIONS[current_cpu()].swap(core::ptr::null_mut(), Ordering::Acquire);
    let Some(session) = NonNull::new(pointer) else {
        return;
    };
    // SAFETY: Registration retains this session on the outgoing Thread's
    // private stack. The switch has not saved or published that stack yet,
    // IRQs are masked, and no scheduler/MM lock remains held. Architecture
    // suspension closes every raw-frame/FP/TLS CPU ownership before MM leave.
    unsafe { crate::hal::user::suspend_current_user() };
    #[cfg(all(
        CONFIG_ARCH_AARCH64,
        all(feature = "kernel-test", feature = "kernel-self-test")
    ))]
    // SAFETY: The registered outgoing session remains retained as above.
    self_test::record_suspension(unsafe { &session.as_ref().session.thread });
    // SAFETY: The outgoing Thread still exclusively owns its registered cell;
    // taking the token empties all CPU-affine state before context migration.
    let active = unsafe { &mut *session.as_ref().active.get() }.take();
    let Some(active) = active else {
        hyper::debug::invariant_failure("Native switch lost its active translation");
    };
    if let Err(error) = active.leave() {
        fail_run("failed to suspend native-user translation", error);
    }
}

/// Fixed scheduler entry for every native user Thread.
/// Returning transfers to the ordinary scheduler thread-exit trampoline.
pub(crate) extern "C" fn thread_entry(_argument: usize) {
    let pin = match scheduler::preempt_disable() {
        Ok(pin) => pin,
        Err(error) => fail_run("failed to attach native-user execution", error),
    };
    let current = match scheduler::current_user(&pin) {
        Ok(current) => current,
        Err(error) => fail_run("failed to identify current native-user Thread", error),
    };
    let (session, execution) = UserSession::attach(current);
    if let Err(error) = scheduler::preempt_enable_without_reschedule(pin) {
        fail_run("failed to release native-user attachment pin", error);
    }
    // SAFETY: The boxed payload belongs to this exact scheduler Thread and
    // cannot be reclaimed until this entry returns. Stop preserves runnable
    // continuations so they close architecture ownership and Rust locals.
    run_session(&session, unsafe { execution.as_ref() });
}

fn run_session(session: &UserSession, execution: &UserExecution) {
    let machine = NativeMachineSession {
        session,
        execution,
        active: UnsafeCell::new(None),
    };
    loop {
        if session.stop_requested() {
            return;
        }
        let prepared = match session
            .thread
            .prepare_run(session.process.image_generation())
        {
            Ok(prepared) => prepared,
            Err(RunAdmissionError::AdmissionClosed) => return,
            Err(error) => fail_run("failed to reserve native-user generation", error),
        };
        if !machine.prepare_user_return() {
            machine.detach_address();
            prepared.abort();
            crate::hal::irq::enable_local();
            return;
        }
        let active_run = match prepared.commit() {
            Ok(active) => active,
            Err(error) => {
                machine.detach_address();
                crate::hal::irq::enable_local();
                if error == RunAdmissionError::AdmissionClosed {
                    return;
                }
                fail_run("failed to publish native-user generation", error);
            }
        };
        let binding = active_run.binding();
        let service = NativeCallService::new(&machine);
        // SAFETY: This Thread retains its scheduler payload, session, service,
        // and admitted generation until the returned capability is consumed.
        // Initial translation is registered and IRQs are masked. The scheduler
        // hook retires CPU state before every switch; direct callback return
        // restores translation. No CPU-affine guard spans this machine run.
        let (exit, architecture) = match unsafe {
            crate::hal::user::run_user(&mut *execution.context_ptr(), binding, &service)
        } {
            Ok(stopped) => stopped,
            Err(error) => fail_run("failed to run native-user context", error),
        };
        let observed_epoch = machine.mapping_epoch();
        machine.detach_address();
        let proof = execution.address_space().finish_user_exit(architecture);
        let stopped_run = active_run.stop_after_machine_exit(proof);
        crate::hal::irq::enable_local();

        let stop_requested = session.stop_requested();
        match exit {
            crate::hal::user::UserExit::Interrupted { completion } => {
                let result = if stop_requested {
                    completion.discard(binding)
                } else {
                    completion.resume_execution(binding)
                };
                if let Err(failure) = result {
                    fail_completion(failure);
                }
                stopped_run.acknowledge_architecture_exit();
                if stop_requested {
                    return;
                }
            }
            crate::hal::user::UserExit::Fault { fault, completion } => {
                let Some(observed_epoch) = observed_epoch else {
                    hyper::debug::invariant_failure("Native fault has no translation epoch");
                };
                let resolution = fault::resolve(
                    &session.thread,
                    execution.address_space(),
                    UserAddress::new(fault.address()),
                    fault.kind(),
                    observed_epoch,
                );
                let retry_conflict = resolution
                    .as_ref()
                    .is_err_and(|error| error.is_mapping_conflict());
                let resolved = matches!(resolution, Ok(true));
                let stopped = session.stop_requested();
                if !stopped && (resolved || retry_conflict) {
                    if let Err(failure) = completion.resume_execution(binding) {
                        fail_completion(failure);
                    }
                    stopped_run.acknowledge_architecture_exit();
                    if retry_conflict && let Err(error) = scheduler::yield_now() {
                        fail_run("failed to yield a contended private-page fault", error);
                    }
                } else {
                    if !stopped {
                        report_user_fault(session, fault, observed_epoch, &resolution);
                        session.process.request_fault_stop(
                            fault_reason(fault),
                            fault.program_counter(),
                            fault.address(),
                            fault.syndrome(),
                        );
                    }
                    if let Err(failure) = completion.discard(binding) {
                        fail_completion(failure);
                    }
                    stopped_run.acknowledge_architecture_exit();
                    return;
                }
            }
        }
    }
}

#[inline(never)]
fn report_user_fault(
    session: &UserSession,
    fault: UserFault,
    observed_epoch: u64,
    resolution: &Result<bool, crate::mm::user_space::MachineError>,
) {
    let name = session.process.name();
    crate::pr_err!(
        "HypeR: Native process fault process={} process_koid={} thread_koid={} kind={:?} pc={:#x} address={:#x} syndrome={:#x} observed_epoch={} resolution={:?}",
        core::str::from_utf8(name.as_bytes()).unwrap_or("<invalid name>"),
        session.process.koid().get(),
        session.thread.object_snapshot().koid.get(),
        fault.kind(),
        fault.program_counter(),
        fault.address(),
        fault.syndrome(),
        observed_epoch,
        resolution,
    );
}

fn current_cpu() -> hyper::cpu::CpuIndex {
    match crate::cpu::current_index() {
        Some(cpu) => cpu,
        None => hyper::debug::invariant_failure("Native continuation lost CPU identity"),
    }
}

fn require_masked() {
    if crate::hal::irq::local_enabled() {
        hyper::debug::invariant_failure("Native transition requires masked IRQs");
    }
}

fn preemption_pending() -> bool {
    require_masked();
    match crate::task::preempt::pending(current_cpu()) {
        Ok(pending) => pending,
        Err(error) => fail_run("failed to inspect native-user preemption state", error),
    }
}

fn validate_current_stack(stack: (usize, usize)) {
    let marker = 0usize;
    let pointer = core::ptr::from_ref(&marker).expose_provenance();
    if pointer < stack.0 || pointer >= stack.1 {
        crate::diagnostics::crash::fatal(format_args!(
            "HypeR: native-user runner is outside its scheduler stack"
        ));
    }
}

fn fault_reason(fault: UserFault) -> TerminalReason {
    let class = match fault.kind() {
        UserFaultKind::InstructionAbort | UserFaultKind::ExecutePageFault => 1,
        UserFaultKind::DataAbort | UserFaultKind::WritePageFault | UserFaultKind::ReadPageFault => {
            2
        }
        UserFaultKind::Alignment => 3,
        UserFaultKind::IllegalInstruction => 4,
        UserFaultKind::SystemAccess => 5,
        UserFaultKind::Breakpoint => 6,
        UserFaultKind::OtherSynchronous => 7,
    };
    TerminalReason::Fault {
        class,
        code: fault.syndrome(),
    }
}

fn fail_completion(failure: crate::hal::user::CompletionFailure<'_>) -> ! {
    failure.abandon_with(|error| fail_run("native-user return ownership is inconsistent", error))
}

fn fail_run(context: &str, error: impl core::fmt::Debug) -> ! {
    crate::diagnostics::crash::fatal(format_args!("HypeR: {context}: {error:?}"))
}
