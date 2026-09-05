// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Cross-CPU scheduler placement and context-handoff contracts.

use hyper::cpu::CpuIndex;
use hyper::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use crate::kernel::sync::Semaphore;
use crate::kernel::task::scheduler::{self, CpuMask, MigrationStatus, ThreadPriority};
use crate::kernel::task::{SleepError, sleep_ms};

const PROGRESS_TIMEOUT_NS: u64 = 2_000_000_000;
const SLEEP_MIGRATION_MS: u64 = 500;

static TARGET_BLOCKER_ENTERED: AtomicBool = AtomicBool::new(false);
static TARGET_BLOCKER_RELEASE: AtomicBool = AtomicBool::new(false);
static READY_STAGE: AtomicUsize = AtomicUsize::new(0);
static READY_FAILURE: AtomicUsize = AtomicUsize::new(0);

static RUNNING_STAGE: AtomicUsize = AtomicUsize::new(0);
static RUNNING_FAILURE: AtomicUsize = AtomicUsize::new(0);
static RUNNING_OWNER: AtomicUsize = AtomicUsize::new(0);
static RUNNING_RELEASE: AtomicBool = AtomicBool::new(false);

static REMOTE_STAGE: AtomicUsize = AtomicUsize::new(0);
static REMOTE_FAILURE: AtomicUsize = AtomicUsize::new(0);
static REMOTE_OWNER: AtomicUsize = AtomicUsize::new(0);
static REMOTE_RELEASE: AtomicBool = AtomicBool::new(false);

static BLOCK_GATE: Semaphore = Semaphore::new(0);
static BLOCK_STAGE: AtomicUsize = AtomicUsize::new(0);
static BLOCK_FAILURE: AtomicUsize = AtomicUsize::new(0);

static SLEEP_STAGE: AtomicUsize = AtomicUsize::new(0);
static SLEEP_FAILURE: AtomicUsize = AtomicUsize::new(0);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Error {
    InvalidCpu,
    Quiescence(super::support::QuiescenceError),
    Scheduler(scheduler::Error),
    Sleep(SleepError),
    Synchronization(crate::kernel::sync::Error),
    Timeout(&'static str),
    StateMismatch(usize),
}

impl From<scheduler::Error> for Error {
    fn from(error: scheduler::Error) -> Self {
        Self::Scheduler(error)
    }
}

impl From<super::support::QuiescenceError> for Error {
    fn from(error: super::support::QuiescenceError) -> Self {
        Self::Quiescence(error)
    }
}

impl From<SleepError> for Error {
    fn from(error: SleepError) -> Self {
        Self::Sleep(error)
    }
}

impl From<crate::kernel::sync::Error> for Error {
    fn from(error: crate::kernel::sync::Error) -> Self {
        Self::Synchronization(error)
    }
}

pub(super) fn run() -> Result<(), Error> {
    exercise_transactional_rejection()?;
    exercise_terminated_rejection()?;

    if crate::kernel::cpu::online_cpu_count() <= 1 {
        crate::println!("HypeR test: cross-CPU thread migration skipped (one CPU online)");
        return Ok(());
    }

    let target = CpuIndex::new(1).ok_or(Error::InvalidCpu)?;
    exercise_ready_migration(target)?;
    exercise_running_migration(target)?;
    if let Some(second_target) = CpuIndex::new(2)
        && crate::kernel::cpu::online_cpu_count() > 2
    {
        exercise_remote_running_migration(target, second_target)?;
    }
    exercise_blocked_migration(target)?;
    exercise_sleep_migration(target)?;
    quiesce_threads()?;
    crate::println!("HypeR test: cross-CPU thread migration passed");
    Ok(())
}

fn exercise_transactional_rejection() -> Result<(), Error> {
    let boot = CpuIndex::BOOT;
    let dormant = scheduler::kthread_create_with_affinity(
        "migration/dormant",
        empty_worker,
        0,
        CpuMask::single(boot),
    )?;
    if scheduler::migrate_thread(dormant, boot)? != MigrationStatus::Completed {
        return Err(Error::StateMismatch(1));
    }
    if scheduler::set_thread_affinity(dormant, CpuMask::single(boot))? != MigrationStatus::Completed
    {
        return Err(Error::StateMismatch(17));
    }
    let initial = scheduler::thread_placement(dormant)?;
    if scheduler::set_thread_affinity(dormant, CpuMask::EMPTY)
        != Err(scheduler::Error::EmptyCpuAffinity)
        || scheduler::thread_placement(dormant)? != initial
    {
        return Err(Error::StateMismatch(2));
    }

    let online = crate::kernel::cpu::online_cpu_count();
    if let Some(offline) = CpuIndex::new(online)
        && (scheduler::migrate_thread(dormant, offline) != Err(scheduler::Error::CpuNotRegistered)
            || scheduler::thread_placement(dormant)? != initial)
    {
        return Err(Error::StateMismatch(3));
    }
    if let Some(target) = CpuIndex::new(1)
        && online > 1
    {
        if scheduler::migrate_thread(dormant, target) != Err(scheduler::Error::CpuNotAllowed)
            || scheduler::thread_placement(dormant)? != initial
        {
            return Err(Error::StateMismatch(4));
        }
        let target_only = CpuMask::single(target);
        if scheduler::set_thread_affinity(dormant, target_only)? != MigrationStatus::Completed
            || scheduler::thread_placement(dormant)? != (target, target_only)
            || scheduler::migrate_thread(dormant, boot) != Err(scheduler::Error::CpuNotAllowed)
            || scheduler::thread_placement(dormant)? != (target, target_only)
        {
            return Err(Error::StateMismatch(18));
        }
    }
    scheduler::discard_dormant_kernel_thread(dormant)?;

    let bootstrap = scheduler::current_thread_id()?;
    if scheduler::migrate_thread(bootstrap, boot) != Err(scheduler::Error::MigrationUnsupported) {
        return Err(Error::StateMismatch(5));
    }
    Ok(())
}

fn exercise_terminated_rejection() -> Result<(), Error> {
    let boot = CpuIndex::BOOT;
    scheduler::set_thread_fifo_policy(scheduler::current_thread_id()?, ThreadPriority::NORMAL)?;
    let thread = scheduler::kthread_create_fifo_with_affinity(
        "migration/terminated",
        empty_worker,
        0,
        ThreadPriority::NORMAL,
        CpuMask::single(boot),
    )?;
    scheduler::thread_ready(thread)?;
    scheduler::yield_now()?;
    // A same-CPU reschedule IPI may run and retire the worker before the
    // explicit yield reaches its reap phase. Both observations prove that an
    // exited identity cannot be migrated; neither is a success path.
    match scheduler::migrate_thread(thread, boot) {
        Err(scheduler::Error::TerminatedThread | scheduler::Error::ThreadNotFound) => {}
        _ => return Err(Error::StateMismatch(6)),
    }
    scheduler::set_thread_fair_policy(scheduler::current_thread_id()?)?;
    quiesce_threads()
}

fn exercise_ready_migration(target: CpuIndex) -> Result<(), Error> {
    TARGET_BLOCKER_ENTERED.store(false, Ordering::Release);
    TARGET_BLOCKER_RELEASE.store(false, Ordering::Release);
    READY_STAGE.store(0, Ordering::Release);
    READY_FAILURE.store(0, Ordering::Release);

    scheduler::set_thread_fifo_policy(scheduler::current_thread_id()?, ThreadPriority::NORMAL)?;
    let blocker = scheduler::kthread_create_fifo_with_affinity(
        "migration/target-blocker",
        target_blocker,
        0,
        ThreadPriority::HIGHEST,
        CpuMask::single(target),
    )?;
    scheduler::thread_ready(blocker)?;
    wait_until("migration target blocker did not run", || {
        TARGET_BLOCKER_ENTERED.load(Ordering::Acquire)
    })?;

    let affinity = CpuMask::EMPTY.with_cpu(CpuIndex::BOOT).with_cpu(target);
    let worker = scheduler::kthread_create_fifo_with_affinity(
        "migration/ready",
        ready_worker,
        target.get(),
        ThreadPriority::NORMAL,
        affinity,
    )?;
    scheduler::thread_ready(worker)?;
    scheduler::yield_now()?;
    if READY_STAGE.load(Ordering::Acquire) != 1 {
        return Err(Error::StateMismatch(7));
    }

    let before = scheduler::statistics()?;
    if scheduler::migrate_thread(worker, target)? != MigrationStatus::Completed {
        return Err(Error::StateMismatch(8));
    }
    let after = scheduler::statistics()?;
    if before.per_cpu_ready[CpuIndex::BOOT.get()] != after.per_cpu_ready[CpuIndex::BOOT.get()] + 1
        || before.per_cpu_ready[target.get()] + 1 != after.per_cpu_ready[target.get()]
        || scheduler::thread_placement(worker)? != (target, affinity)
    {
        return Err(Error::StateMismatch(9));
    }

    TARGET_BLOCKER_RELEASE.store(true, Ordering::Release);
    wait_until(
        "ready Thread did not resume on its migration target",
        || READY_STAGE.load(Ordering::Acquire) == 2,
    )?;
    if READY_FAILURE.load(Ordering::Acquire) != 0 {
        return Err(Error::StateMismatch(10));
    }
    scheduler::set_thread_fair_policy(scheduler::current_thread_id()?)?;
    quiesce_threads()
}

fn exercise_running_migration(target: CpuIndex) -> Result<(), Error> {
    RUNNING_STAGE.store(0, Ordering::Release);
    RUNNING_FAILURE.store(0, Ordering::Release);
    RUNNING_OWNER.store(0, Ordering::Release);
    RUNNING_RELEASE.store(false, Ordering::Release);
    scheduler::set_thread_fifo_policy(scheduler::current_thread_id()?, ThreadPriority::NORMAL)?;

    #[cfg(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_X86_64))]
    let delivery_before = crate::kernel::irq::reschedule_delivery_count_for_test(target);
    let affinity = CpuMask::EMPTY.with_cpu(CpuIndex::BOOT).with_cpu(target);
    let worker = scheduler::kthread_create_fifo_with_affinity(
        "migration/running",
        running_worker,
        target.get(),
        ThreadPriority::NORMAL,
        affinity,
    )?;
    scheduler::thread_ready(worker)?;
    scheduler::yield_now()?;
    wait_until(
        "running Thread did not resume on its migration target",
        || {
            RUNNING_STAGE.load(Ordering::Acquire) == 2
                || RUNNING_FAILURE.load(Ordering::Acquire) != 0
        },
    )?;
    let failure = RUNNING_FAILURE.load(Ordering::Acquire);
    if failure != 0 {
        return Err(Error::StateMismatch(110 + failure));
    }
    if scheduler::thread_placement(worker)? != (target, CpuMask::single(target)) {
        return Err(Error::StateMismatch(119));
    }
    RUNNING_RELEASE.store(true, Ordering::Release);
    #[cfg(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_X86_64))]
    if crate::kernel::irq::reschedule_delivery_count_for_test(target) <= delivery_before {
        return Err(Error::StateMismatch(12));
    }
    scheduler::set_thread_fair_policy(scheduler::current_thread_id()?)?;
    quiesce_threads()
}

fn exercise_blocked_migration(target: CpuIndex) -> Result<(), Error> {
    BLOCK_STAGE.store(0, Ordering::Release);
    BLOCK_FAILURE.store(0, Ordering::Release);
    scheduler::set_thread_fifo_policy(scheduler::current_thread_id()?, ThreadPriority::NORMAL)?;
    let affinity = CpuMask::EMPTY.with_cpu(CpuIndex::BOOT).with_cpu(target);
    let worker = scheduler::kthread_create_fifo_with_affinity(
        "migration/blocked",
        blocked_worker,
        target.get(),
        ThreadPriority::NORMAL,
        affinity,
    )?;
    scheduler::thread_ready(worker)?;
    scheduler::yield_now()?;
    if BLOCK_STAGE.load(Ordering::Acquire) != 1
        || scheduler::migrate_thread(worker, target)? != MigrationStatus::Completed
        || scheduler::thread_placement(worker)? != (target, affinity)
    {
        return Err(Error::StateMismatch(13));
    }
    BLOCK_GATE.release()?;
    wait_until(
        "blocked Thread did not wake on its migration target",
        || BLOCK_STAGE.load(Ordering::Acquire) == 2,
    )?;
    if BLOCK_FAILURE.load(Ordering::Acquire) != 0 {
        return Err(Error::StateMismatch(14));
    }
    scheduler::set_thread_fair_policy(scheduler::current_thread_id()?)?;
    quiesce_threads()
}

fn exercise_remote_running_migration(source: CpuIndex, target: CpuIndex) -> Result<(), Error> {
    REMOTE_STAGE.store(0, Ordering::Release);
    REMOTE_FAILURE.store(0, Ordering::Release);
    REMOTE_OWNER.store(0, Ordering::Release);
    REMOTE_RELEASE.store(false, Ordering::Release);
    let affinity = CpuMask::EMPTY.with_cpu(source).with_cpu(target);

    let worker = scheduler::kthread_create_fifo_with_affinity(
        "migration/remote-running",
        remote_running_worker,
        target.get(),
        ThreadPriority::NORMAL,
        affinity,
    )?;
    scheduler::thread_ready(worker)?;
    wait_until("remote migration worker did not start", || {
        REMOTE_STAGE.load(Ordering::Acquire) == 1
    })?;
    // Starting the worker already sends a source reschedule prompt. Snapshot
    // only after it is running so the following deltas prove migration's own
    // source and target notifications.
    #[cfg(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_X86_64))]
    let source_delivery = crate::kernel::irq::reschedule_delivery_count_for_test(source);
    #[cfg(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_X86_64))]
    let target_delivery = crate::kernel::irq::reschedule_delivery_count_for_test(target);
    if scheduler::migrate_thread(worker, target)? != MigrationStatus::Pending {
        return Err(Error::StateMismatch(19));
    }
    wait_until(
        "remotely requested running migration did not complete",
        || REMOTE_STAGE.load(Ordering::Acquire) == 2 || REMOTE_FAILURE.load(Ordering::Acquire) != 0,
    )?;
    let failure = REMOTE_FAILURE.load(Ordering::Acquire);
    if failure != 0 {
        return Err(Error::StateMismatch(200 + failure));
    }
    if scheduler::thread_placement(worker)? != (target, affinity) {
        return Err(Error::StateMismatch(209));
    }
    #[cfg(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_X86_64))]
    wait_until(
        "running migration source or target IPI was not delivered",
        || {
            crate::kernel::irq::reschedule_delivery_count_for_test(source) > source_delivery
                && crate::kernel::irq::reschedule_delivery_count_for_test(target) > target_delivery
        },
    )?;
    REMOTE_RELEASE.store(true, Ordering::Release);
    quiesce_threads()
}

fn exercise_sleep_migration(target: CpuIndex) -> Result<(), Error> {
    SLEEP_STAGE.store(0, Ordering::Release);
    SLEEP_FAILURE.store(0, Ordering::Release);
    scheduler::set_thread_fifo_policy(scheduler::current_thread_id()?, ThreadPriority::NORMAL)?;
    let affinity = CpuMask::EMPTY.with_cpu(CpuIndex::BOOT).with_cpu(target);
    let worker = scheduler::kthread_create_fifo_with_affinity(
        "migration/sleep",
        sleep_worker,
        target.get(),
        ThreadPriority::NORMAL,
        affinity,
    )?;
    scheduler::thread_ready(worker)?;
    scheduler::yield_now()?;
    if SLEEP_STAGE.load(Ordering::Acquire) != 1
        || scheduler::migrate_thread(worker, target)? != MigrationStatus::Completed
        || scheduler::thread_placement(worker)? != (target, affinity)
    {
        return Err(Error::StateMismatch(15));
    }
    wait_until(
        "sleeping Thread did not wake on its migration target",
        || SLEEP_STAGE.load(Ordering::Acquire) == 2,
    )?;
    if SLEEP_FAILURE.load(Ordering::Acquire) != 0 {
        return Err(Error::StateMismatch(16));
    }
    scheduler::set_thread_fair_policy(scheduler::current_thread_id()?)?;
    quiesce_threads()
}

fn quiesce_threads() -> Result<(), Error> {
    super::support::quiesce_workers()?;
    Ok(())
}

fn wait_until(description: &'static str, condition: impl FnMut() -> bool) -> Result<(), Error> {
    let reached = crate::kernel::time::spin_wait_until(PROGRESS_TIMEOUT_NS, condition)
        .map_err(|_| Error::Timeout("migration progress clock failed"))?;
    if reached {
        Ok(())
    } else {
        Err(Error::Timeout(description))
    }
}

fn record_failure(slot: &AtomicUsize, code: usize) {
    let _ = slot.compare_exchange(0, code, Ordering::AcqRel, Ordering::Acquire);
}

extern "C" fn empty_worker(_argument: usize) {}

extern "C" fn target_blocker(_argument: usize) {
    TARGET_BLOCKER_ENTERED.store(true, Ordering::Release);
    while !TARGET_BLOCKER_RELEASE.load(Ordering::Acquire) {
        core::hint::spin_loop();
    }
}

extern "C" fn ready_worker(target: usize) {
    if crate::kernel::cpu::current_index() != Some(CpuIndex::BOOT) {
        record_failure(&READY_FAILURE, 1);
    }
    READY_STAGE.store(1, Ordering::Release);
    if scheduler::yield_now().is_err() {
        record_failure(&READY_FAILURE, 2);
        return;
    }
    if crate::kernel::cpu::current_index().map(CpuIndex::get) != Some(target) {
        record_failure(&READY_FAILURE, 3);
    }
    READY_STAGE.store(2, Ordering::Release);
}

extern "C" fn running_worker(target: usize) {
    if RUNNING_OWNER
        .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        record_failure(&RUNNING_FAILURE, 1);
        return;
    }
    let id = match scheduler::current_thread_id() {
        Ok(id) => id,
        Err(_) => {
            record_failure(&RUNNING_FAILURE, 2);
            return;
        }
    };
    let target_cpu = match CpuIndex::new(target) {
        Some(cpu) => cpu,
        None => {
            record_failure(&RUNNING_FAILURE, 3);
            return;
        }
    };
    if scheduler::set_thread_affinity(id, CpuMask::single(target_cpu))
        != Ok(MigrationStatus::Pending)
    {
        record_failure(&RUNNING_FAILURE, 4);
        return;
    }
    RUNNING_STAGE.store(1, Ordering::Release);
    while crate::kernel::cpu::current_index() != Some(target_cpu) {
        if scheduler::cond_resched().is_err() {
            record_failure(&RUNNING_FAILURE, 5);
            return;
        }
        core::hint::spin_loop();
    }
    if RUNNING_OWNER
        .compare_exchange(1, 2, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        record_failure(&RUNNING_FAILURE, 6);
        return;
    }
    if crate::kernel::cpu::current_index() != Some(target_cpu) {
        record_failure(&RUNNING_FAILURE, 7);
    }
    RUNNING_OWNER.store(0, Ordering::Release);
    RUNNING_STAGE.store(2, Ordering::Release);
    while !RUNNING_RELEASE.load(Ordering::Acquire) {
        core::hint::spin_loop();
    }
}

extern "C" fn blocked_worker(target: usize) {
    BLOCK_STAGE.store(1, Ordering::Release);
    if BLOCK_GATE.acquire().is_err() {
        record_failure(&BLOCK_FAILURE, 1);
        return;
    }
    if crate::kernel::cpu::current_index().map(CpuIndex::get) != Some(target) {
        record_failure(&BLOCK_FAILURE, 2);
    }
    BLOCK_STAGE.store(2, Ordering::Release);
}

extern "C" fn remote_running_worker(target: usize) {
    if crate::kernel::cpu::current_index() != CpuIndex::new(1)
        || REMOTE_OWNER
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
    {
        record_failure(&REMOTE_FAILURE, 1);
        return;
    }
    REMOTE_STAGE.store(1, Ordering::Release);
    let Some(target_cpu) = CpuIndex::new(target) else {
        record_failure(&REMOTE_FAILURE, 2);
        return;
    };
    while crate::kernel::cpu::current_index() != Some(target_cpu) {
        #[cfg(CONFIG_ARCH_AARCH64)]
        core::hint::spin_loop();
        #[cfg(not(CONFIG_ARCH_AARCH64))]
        if scheduler::cond_resched().is_err() {
            record_failure(&REMOTE_FAILURE, 2);
            return;
        }
        #[cfg(not(CONFIG_ARCH_AARCH64))]
        core::hint::spin_loop();
    }
    if REMOTE_OWNER
        .compare_exchange(1, 2, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
        || crate::kernel::cpu::current_index() != Some(target_cpu)
    {
        record_failure(&REMOTE_FAILURE, 3);
    }
    REMOTE_OWNER.store(0, Ordering::Release);
    REMOTE_STAGE.store(2, Ordering::Release);
    while !REMOTE_RELEASE.load(Ordering::Acquire) {
        core::hint::spin_loop();
    }
}

extern "C" fn sleep_worker(target: usize) {
    SLEEP_STAGE.store(1, Ordering::Release);
    if sleep_ms(SLEEP_MIGRATION_MS).is_err() {
        record_failure(&SLEEP_FAILURE, 1);
        return;
    }
    if crate::kernel::cpu::current_index().map(CpuIndex::get) != Some(target) {
        record_failure(&SLEEP_FAILURE, 2);
    }
    SLEEP_STAGE.store(2, Ordering::Release);
}
