// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Actual macro/CPU/clock integration, including local IRQ-mask preservation.

#[cfg(feature = "kernel-trace")]
use hyper::sync::atomic::{AtomicBool, Ordering};
#[cfg(feature = "kernel-trace")]
static FAILED: AtomicBool = AtomicBool::new(false);
#[cfg(feature = "kernel-trace")]
static IRQ_DONE: [AtomicBool; hyper::cpu::MAX_CPUS] =
    [const { AtomicBool::new(false) }; hyper::cpu::MAX_CPUS];

#[cfg(not(feature = "kernel-trace"))]
pub(super) fn run() -> Result<(), &'static str> {
    crate::tracepoint!(
        Test,
        "compiled_out",
        value = unavailable_without_instrumentation()
    );
    Ok(())
}

#[cfg(feature = "kernel-trace")]
pub(super) fn run() -> Result<(), &'static str> {
    use crate::kernel::{
        task::scheduler::{self, CpuMask},
        trace,
    };
    use hyper::{
        cpu::CpuIndex,
        trace::{Category, Filter},
    };
    FAILED.store(false, Ordering::Relaxed);
    for done in &IRQ_DONE {
        done.store(false, Ordering::Relaxed);
    }
    let session = trace::start(Filter::only(Category::Test)).map_err(|_| "trace start")?;
    for index in 0..crate::kernel::cpu::online_cpu_count() {
        let cpu = CpuIndex::new(index).ok_or("trace CPU")?;
        let thread = scheduler::kthread_create_with_affinity(
            "trace/test",
            writer,
            index,
            CpuMask::single(cpu),
        )
        .map_err(|_| "trace thread")?;
        scheduler::thread_ready(thread).map_err(|_| "trace ready")?;
    }
    super::support::quiesce_workers().map_err(|_| "trace worker quiescence")?;
    if FAILED.load(Ordering::Acquire) {
        return Err("trace IRQ/mask check");
    }
    trace::set_filter(session, Filter::NONE).map_err(|_| "trace pause")?;
    crate::tracepoint!(Test, "paused", value = panic_if_evaluated());
    trace::stop(session).map_err(|_| "trace stop")?;
    let mut records = [None; 8];
    for cpu in 0..crate::kernel::cpu::online_cpu_count() {
        let batch = trace::read(session, cpu, 0, &mut records).map_err(|_| "trace read")?;
        if batch.count != 3 || batch.incomplete || batch.overwritten != 0 {
            return Err("trace count/loss");
        }
        for record in records.iter().take(batch.count).flatten() {
            if record.values[0] != cpu as u64 || record.timestamp_ns == 0 {
                return Err("trace CPU/clock");
            }
        }
    }
    Ok(())
}

#[cfg(feature = "kernel-trace")]
extern "C" fn writer(cpu: usize) {
    let enabled = crate::hal::irq::local_enabled();
    crate::tracepoint!(Test, "thread", cpu = cpu as u64);
    if crate::hal::irq::local_enabled() != enabled {
        FAILED.store(true, Ordering::Release);
    }
    // Exercise the same call site while a caller already owns the IRQ mask.
    // SAFETY: No blocking or migration in this lexical scope.
    let mask = unsafe { hyper::sync::InterruptMaskGuard::<crate::hal::irq::LocalMask>::acquire() };
    crate::tracepoint!(Test, "masked", cpu = cpu as u64);
    if crate::hal::irq::local_enabled() {
        FAILED.store(true, Ordering::Release);
    }
    drop(mask);
    if crate::hal::irq::local_enabled() != enabled || exercise_irq(cpu).is_err() {
        FAILED.store(true, Ordering::Release);
    }
}

#[cfg(feature = "kernel-trace")]
fn exercise_irq(cpu: usize) -> Result<(), &'static str> {
    use crate::kernel::{
        task,
        time::{self, IrqCallout},
    };
    let timer =
        hyper::mm::FallibleArc::try_new(IrqCallout::try_new().map_err(|_| "reserve IRQ timer")?)
            .map_err(|_| "allocate IRQ timer")?;
    let deadline = time::deadline_after(1_000_000).map_err(|_| "IRQ timer deadline")?;
    let arm =
        IrqCallout::arm_owned(timer, deadline, irq_event, cpu).map_err(|_| "arm IRQ timer")?;
    let progress = task::wait_for_test_progress(task::TEST_PROGRESS_TIMEOUT_NS, || {
        Ok::<_, task::SleepError>(IRQ_DONE[cpu].load(Ordering::Acquire))
    });
    arm.retire().map_err(|_| "retire IRQ timer")?;
    if progress.map_err(|_| "IRQ timer wait")? {
        Ok(())
    } else {
        Err("IRQ timer timeout")
    }
}

#[cfg(feature = "kernel-trace")]
fn irq_event(expected: usize) {
    if let Some(cpu) = crate::kernel::cpu::current_index() {
        if cpu.get() != expected {
            FAILED.store(true, Ordering::Release);
        }
        crate::tracepoint!(Test, "timer_irq", cpu = cpu.get() as u64);
    }
    IRQ_DONE[expected].store(true, Ordering::Release);
}

#[cfg(feature = "kernel-trace")]
fn panic_if_evaluated() -> u64 {
    crate::kernel::crash::fatal(format_args!("disabled trace evaluated its payload"))
}
