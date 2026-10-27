// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! One-shot gates for syscall IRQ suspension and contended MM detach tests.

use crate::capability::handle_wire;

use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use hyper::abi::native::{HYPER_NATIVE_OP_ABI_QUERY, NativeInvocation};
use hyper::cpu::CpuIndex;
use hyper::sync::InterruptMaskGuard;

use crate::capability::Rights;
use crate::mm::user_space::NativeAddressSpace;
use crate::object::ObjectKind;
use {crate::process::Process, crate::process::UserThread};
use {crate::task, crate::task::scheduler, crate::task::thread::ThreadId};

const NO_CPU: usize = usize::MAX;

struct State {
    taken: AtomicBool,
    target: AtomicU64,
    entered_cpu: AtomicUsize,
    suspended: AtomicBool,
    resumed_cpu: AtomicUsize,
    released: AtomicBool,
    timed_out: AtomicBool,
}

impl State {
    const fn new() -> Self {
        Self {
            taken: AtomicBool::new(false),
            target: AtomicU64::new(0),
            entered_cpu: AtomicUsize::new(NO_CPU),
            suspended: AtomicBool::new(false),
            resumed_cpu: AtomicUsize::new(NO_CPU),
            released: AtomicBool::new(false),
            timed_out: AtomicBool::new(false),
        }
    }

    fn matches(&self, thread: &UserThread) -> bool {
        thread.scheduler_id().map(ThreadId::get) == Some(self.target.load(Ordering::Acquire))
    }

    fn armed(&self, thread: &UserThread) -> bool {
        self.matches(thread)
            && self.entered_cpu.load(Ordering::Relaxed) == NO_CPU
            && !self.released.load(Ordering::Acquire)
    }

    fn enter(&self) -> bool {
        // SAFETY: This short mask only qualifies and publishes the original
        // syscall's resident bank. It ends before either interruptible spin.
        let mask = unsafe { InterruptMaskGuard::<crate::hal::irq::LocalMask>::acquire() };
        if !crate::hal::user::syscall_fp_resident_for_test(&mask) {
            return false;
        }
        self.entered_cpu
            .store(super::current_cpu().get(), Ordering::Release);
        true
    }

    fn wait_for_release(&self) {
        let completed = crate::time::spin_wait_until(task::TEST_PROGRESS_TIMEOUT_NS, || {
            if self.suspended.load(Ordering::Acquire) {
                self.resumed_cpu
                    .store(super::current_cpu().get(), Ordering::Release);
            }
            self.released.load(Ordering::Acquire)
        });
        if !matches!(completed, Ok(true)) {
            self.timed_out.store(true, Ordering::Release);
        }
    }
}

static IRQ: State = State::new();
static BUSY: State = State::new();

/// Drop releases the callback before any controller failure can retire it.
pub(crate) struct Gate(&'static State);

impl Gate {
    pub(crate) fn arm(thread: ThreadId) -> Option<Self> {
        Self::arm_state(&IRQ, thread)
    }

    pub(crate) fn arm_busy(thread: ThreadId) -> Option<Self> {
        Self::arm_state(&BUSY, thread)
    }

    fn arm_state(state: &'static State, thread: ThreadId) -> Option<Self> {
        if thread == ThreadId::BOOTSTRAP
            || state
                .taken
                .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
                .is_err()
        {
            return None;
        }
        // Each mode is used once per boot: a delayed continuation cannot
        // observe a later test's state. The scheduler identity is never reused.
        state.target.store(thread.get(), Ordering::Release);
        Some(Self(state))
    }

    pub(crate) fn entered_cpu(&self) -> Option<CpuIndex> {
        CpuIndex::new(self.0.entered_cpu.load(Ordering::Acquire))
    }

    pub(crate) fn resumed_cpu(&self) -> Option<CpuIndex> {
        CpuIndex::new(self.0.resumed_cpu.load(Ordering::Acquire))
    }

    pub(crate) fn timed_out(&self) -> bool {
        self.0.timed_out.load(Ordering::Acquire)
    }

    pub(crate) fn release(&self) {
        self.0.released.store(true, Ordering::Release);
    }
}

impl Drop for Gate {
    fn drop(&mut self) {
        self.release();
        self.0.target.store(0, Ordering::Release);
    }
}

pub(super) fn hold_syscall(process: &Process, thread: &UserThread, invocation: NativeInvocation) {
    if invocation.operation() != HYPER_NATIVE_OP_ABI_QUERY
        || !(IRQ.armed(thread) || BUSY.armed(thread))
    {
        return;
    }
    let Ok(handle) = handle_wire::from_abi(invocation.target()) else {
        return;
    };
    if !process
        .handle_info(handle, Rights::NONE)
        .is_ok_and(|info| info.kind == ObjectKind::SYSTEM_INFO)
    {
        return;
    }
    if IRQ.armed(thread) && IRQ.enter() {
        // No wait/yield or preemption pin: suspension can only happen through
        // a host IRQ tail, whose frame has FP disabled while the original
        // qualified user frame has FP enabled.
        IRQ.wait_for_release();
    }
    if BUSY.armed(thread) {
        hold_for_mm_cut(thread);
    }
}

fn hold_for_mm_cut(thread: &UserThread) {
    let Some(identity) = thread.scheduler_id() else {
        return;
    };
    let Ok(pin) = scheduler::preempt_disable() else {
        BUSY.timed_out.store(true, Ordering::Release);
        return;
    };
    let entered = BUSY.enter();
    let closed = entered
        && matches!(
            crate::time::spin_wait_until(task::TEST_PROGRESS_TIMEOUT_NS, || {
                BUSY.released.load(Ordering::Acquire)
                    || NativeAddressSpace::busy_cut_closed_for_test(identity)
            }),
            Ok(true)
        );
    // The setup pin prevents timer preemption from detaching the root before
    // the remote update closes admission. It ends before the actual switch.
    if let Err(error) = scheduler::preempt_enable_without_reschedule(pin) {
        super::fail_run("failed to release Native MM test setup pin", error);
    }
    if !entered {
        return;
    }
    if !closed {
        BUSY.timed_out.store(true, Ordering::Release);
        return;
    }
    if BUSY.released.load(Ordering::Acquire) {
        return;
    }
    if let Err(error) = scheduler::yield_now() {
        super::fail_run("failed to switch Native MM test continuation", error);
    }
    BUSY.wait_for_release();
}

/// Runs after the exact outgoing Thread's HAL suspension, before MM leave.
pub(super) fn record_suspension(thread: &UserThread) {
    for state in [&IRQ, &BUSY] {
        if state.matches(thread)
            && state.entered_cpu.load(Ordering::Acquire) != NO_CPU
            && !state.released.load(Ordering::Acquire)
        {
            state.suspended.store(true, Ordering::Release);
        }
    }
}
