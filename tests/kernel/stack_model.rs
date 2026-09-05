// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Guard-page, watermark, thread-stack, and IRQ-stack integration tests.

use core::hint::black_box;
use core::ptr::write_volatile;

use hyper::sync::atomic::{AtomicUsize, Ordering};

use crate::kernel::mm::stack;
use crate::kernel::sync::Semaphore;
use crate::kernel::task::scheduler;

static IRQ_CALLBACK_SP: AtomicUsize = AtomicUsize::new(0);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Error {
    GuardMapped,
    IrqCallbackMissing,
    IrqStackMismatch,
    PageAccountingUnavailable,
    Quiescence(super::support::QuiescenceError),
    StackPageAllocation {
        expected: usize,
        observed: usize,
    },
    StackPageRetirement {
        expected: usize,
        observed: usize,
    },
    WorkerNotParked,
    #[cfg(CONFIG_ARCH_X86_64)]
    ShootdownMissing,
    Scheduler(scheduler::Error),
    Sleep(crate::kernel::task::SleepError),
    Stack(stack::Error),
    StackUsageMissing,
    Synchronization(crate::kernel::sync::Error),
}

impl From<scheduler::Error> for Error {
    fn from(error: scheduler::Error) -> Self {
        Self::Scheduler(error)
    }
}

impl From<crate::kernel::task::SleepError> for Error {
    fn from(error: crate::kernel::task::SleepError) -> Self {
        Self::Sleep(error)
    }
}

impl From<super::support::QuiescenceError> for Error {
    fn from(error: super::support::QuiescenceError) -> Self {
        Self::Quiescence(error)
    }
}

impl From<stack::Error> for Error {
    fn from(error: stack::Error) -> Self {
        Self::Stack(error)
    }
}

impl From<crate::kernel::sync::Error> for Error {
    fn from(error: crate::kernel::sync::Error) -> Self {
        Self::Synchronization(error)
    }
}

struct ThreadProbe {
    ready: Semaphore,
    release: Semaphore,
}

impl ThreadProbe {
    const fn new() -> Self {
        Self {
            ready: Semaphore::new(0),
            release: Semaphore::new(0),
        }
    }
}

static THREAD_PROBE: ThreadProbe = ThreadProbe::new();

pub(super) fn run() -> Result<(), Error> {
    validate_cpu_exception_stacks()?;
    validate_thread_stack()?;
    validate_irq_stack_switch()?;
    Ok(())
}

fn validate_cpu_exception_stacks() -> Result<(), Error> {
    let cpu = crate::kernel::cpu::current_index().ok_or(Error::StackUsageMissing)?;
    let (irq, emergency) =
        stack::cpu_stack_statistics(cpu.get()).ok_or(Error::StackUsageMissing)?;
    for statistics in [irq, emergency] {
        if !statistics.canary_intact || !stack::guard_page_is_unmapped(statistics)? {
            return Err(Error::GuardMapped);
        }
    }
    Ok(())
}

fn validate_thread_stack() -> Result<(), Error> {
    #[cfg(CONFIG_ARCH_X86_64)]
    let shootdown_baseline = crate::hal::memory::stage1_shootdown_count_for_test();
    // Exercise dormant ownership retirement, then move any persistent Thread
    // registry growth before the stack-only accounting window. Registry slots
    // intentionally outlive individual Threads and are not stack leakage.
    let warmup = scheduler::kthread_create("stack-accounting-warmup", empty_worker, 0)?;
    scheduler::discard_dormant_kernel_thread(warmup)?;
    super::support::quiesce_workers()?;
    let baseline_pages = crate::kernel::mm::statistics()
        .ok_or(Error::PageAccountingUnavailable)?
        .runtime
        .kernel_pages
        .pages;
    let id = scheduler::kthread_create(
        "stack-watermark",
        stack_worker,
        core::ptr::from_ref(&THREAD_PROBE).expose_provenance(),
    )?;
    scheduler::thread_ready(id)?;
    scheduler::yield_now()?;
    THREAD_PROBE.ready.acquire()?;
    wait_until_worker_parked()?;
    let statistics = scheduler::thread_stack_statistics(id)?.ok_or(Error::StackUsageMissing)?;
    let allocated_pages = crate::kernel::mm::statistics()
        .ok_or(Error::PageAccountingUnavailable)?
        .runtime
        .kernel_pages
        .pages;
    let expected_pages =
        hyper::config::KERNEL_STACK_SIZE_KB as usize * 1024 / hyper::mm::PAGE_SIZE as usize;
    if allocated_pages != baseline_pages + expected_pages {
        return Err(Error::StackPageAllocation {
            expected: baseline_pages + expected_pages,
            observed: allocated_pages,
        });
    }
    if statistics.used < 8 * 1024 || !statistics.canary_intact {
        return Err(Error::StackUsageMissing);
    }
    if !stack::guard_page_is_unmapped(statistics)? {
        return Err(Error::GuardMapped);
    }
    THREAD_PROBE.release.release()?;
    super::support::quiesce_workers()?;
    let final_pages = crate::kernel::mm::statistics()
        .ok_or(Error::PageAccountingUnavailable)?
        .runtime
        .kernel_pages
        .pages;
    if final_pages != baseline_pages {
        return Err(Error::StackPageRetirement {
            expected: baseline_pages,
            observed: final_pages,
        });
    }
    #[cfg(CONFIG_ARCH_X86_64)]
    if crate::hal::memory::stage1_shootdown_count_for_test() <= shootdown_baseline {
        return Err(Error::ShootdownMissing);
    }
    Ok(())
}

fn wait_until_worker_parked() -> Result<(), Error> {
    // The ready notification is published before the worker enters its
    // blocking acquire. Wait until the queue owns the worker before inspecting
    // its stopped stack.
    if crate::kernel::task::wait_for_test_progress(
        crate::kernel::task::TEST_PROGRESS_TIMEOUT_NS,
        || Ok::<_, Error>(THREAD_PROBE.release.waiter_count()? == 1),
    )? {
        Ok(())
    } else {
        Err(Error::WorkerNotParked)
    }
}

extern "C" fn stack_worker(argument: usize) {
    // SAFETY: Only the address of the static THREAD_PROBE is passed here. Its
    // lifetime dominates the one-shot self-test worker and crash-stop paths.
    let probe = unsafe { &*core::ptr::with_exposed_provenance::<ThreadProbe>(argument) };
    let mut consumption = [0u8; 8 * 1024];
    for (index, byte) in consumption.iter_mut().enumerate() {
        // SAFETY: byte is one unique element in the local stack allocation.
        unsafe { write_volatile(byte, index as u8) };
    }
    black_box(&consumption);
    let _ = probe.ready.release();
    let _ = probe.release.acquire();
    black_box(&consumption);
}

extern "C" fn empty_worker(_argument: usize) {}

fn validate_irq_stack_switch() -> Result<(), Error> {
    IRQ_CALLBACK_SP.store(0, Ordering::Release);
    crate::kernel::time::schedule_after(1, record_irq_stack, 0)
        .map_err(|_| Error::IrqCallbackMissing)?;
    let delivered = crate::kernel::task::wait_for_test_progress(
        crate::kernel::task::TEST_PROGRESS_TIMEOUT_NS,
        || Ok::<_, Error>(IRQ_CALLBACK_SP.load(Ordering::Acquire) != 0),
    )?;
    let pointer = IRQ_CALLBACK_SP.load(Ordering::Acquire);
    if !delivered || pointer == 0 {
        return Err(Error::IrqCallbackMissing);
    }
    let cpu = crate::kernel::cpu::current_index().ok_or(Error::StackUsageMissing)?;
    let (irq, _) = stack::cpu_stack_statistics(cpu.get()).ok_or(Error::StackUsageMissing)?;
    if pointer < irq.bottom || pointer > irq.top || irq.used == 0 || !irq.canary_intact {
        return Err(Error::IrqStackMismatch);
    }
    Ok(())
}

fn record_irq_stack(_event: crate::kernel::time::TimerEvent, _context: usize) {
    let marker = 0usize;
    IRQ_CALLBACK_SP.store((&marker as *const usize) as usize, Ordering::Release);
}
