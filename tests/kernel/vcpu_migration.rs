// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Real guest execution across affinity-driven CPU migration and administrative stop.

use crate::kernel::task::scheduler;
use core::sync::atomic::Ordering;
use hyper::cpu::CpuIndex;

// x0 is the shared counter's IPA. STLR pairs with the host's atomic load;
// the counter register itself must survive every exception and migration.
const BUSY: [u32; 4] = [
    0xd2800001, // mov x1, #0
    0x91000421, // add x1, x1, #1
    0xc89ffc01, // stlr x1, [x0]
    0x17fffffe, // b counter loop
];

// Leave guest IRQs masked and arm the virtual timer before WFI. The endpoint
// timer must wake this vCPU even after its blocked Thread changes physical CPU.
const TIMER: [u32; 12] = [
    0xd2800001, // mov x1, #0
    0xd53be002, // mrs x2, cntfrq_el0
    0xd2800143, // mov x3, #10
    0x9ac30842, // udiv x2, x2, x3; 100 ms
    0xd2800023, // mov x3, #1
    0xd51be302, // msr cntv_tval_el0, x2
    0xd51be323, // msr cntv_ctl_el0, x3
    0xd5033fdf, // isb
    0xd503207f, // wfi
    0x91000421, // add x1, x1, #1
    0xc89ffc01, // stlr x1, [x0]
    0x17fffffa, // b timer arm
];

pub(super) enum Error {
    Fixture(super::vm_registry::Error),
    Registry(crate::kernel::vm::registry::Error),
    Installed(&'static str),
    Quiescence(super::support::QuiescenceError),
    Scheduler(scheduler::Error),
    Sleep(crate::kernel::task::SleepError),
    InvalidCpu,
    AffinityContract,
    Timeout,
    Identifier(crate::kernel::mm::translation_id::Error),
}

impl core::fmt::Debug for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Fixture(error) => f.debug_tuple("Fixture").field(error).finish(),
            Self::Registry(error) => f.debug_tuple("Registry").field(error).finish(),
            Self::Installed(error) => f.debug_tuple("Installed").field(error).finish(),
            Self::Quiescence(error) => f.debug_tuple("Quiescence").field(error).finish(),
            Self::Scheduler(error) => f.debug_tuple("Scheduler").field(error).finish(),
            Self::Sleep(error) => f.debug_tuple("Sleep").field(error).finish(),
            Self::InvalidCpu => f.write_str("InvalidCpu"),
            Self::AffinityContract => f.write_str("AffinityContract"),
            Self::Timeout => f.write_str("Timeout"),
            Self::Identifier(error) => f.debug_tuple("Identifier").field(error).finish(),
        }
    }
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

pub(super) fn run() -> Result<(), Error> {
    if !crate::hal::vm::guest_execution_available() || crate::kernel::cpu::online_cpu_count() < 2 {
        crate::pr_notice!(
            "HypeR test: vCPU migration skipped (requires guest execution and two CPUs)"
        );
        return Ok(());
    }
    exercise(&BUSY)?;
    exercise(&TIMER)?;
    crate::pr_info!("HypeR test: running and timer-waiting vCPU migration and retirement passed");
    Ok(())
}

fn exercise(program: &[u32]) -> Result<(), Error> {
    let mut code = [0u8; TIMER.len() * 4];
    for (bytes, instruction) in code.chunks_exact_mut(4).zip(program) {
        bytes.copy_from_slice(&instruction.to_le_bytes());
    }
    let (prepared, domain, counter) =
        super::vm_registry::prepare_migration_guest(&code[..program.len() * 4])
            .map_err(Error::Fixture)?;
    let installed = prepared.install().map_err(Error::Registry)?;
    let running = installed.start_boot_for_test().map_err(Error::Installed)?;
    let thread = running.thread();
    let result = (|| {
        // SAFETY: The installed VM owner retains this initialized, aligned RAM
        // throughout the closure. Guest accesses are same-width STLR stores;
        // only atomic host loads occur until all observation has finished.
        let counter = unsafe { &*counter };
        wait(|| Ok(counter.load(Ordering::Acquire) > 0))?;
        verify_affinity_contract(thread)?;
        let width = crate::hal::vm::guest_translation_identifier_bits()
            .map_err(|_| Error::AffinityContract)?;
        crate::kernel::mm::translation_id::test_rollover::<
            crate::kernel::mm::translation_id::Stage2Vmid,
        >(width)
        .map_err(Error::Identifier)?;
        for _ in 0..8 {
            let (source, _) = scheduler::thread_migration_state(thread)?;
            let target =
                CpuIndex::new(if source.get() == 0 { 1 } else { 0 }).ok_or(Error::InvalidCpu)?;
            scheduler::set_thread_affinity(thread, scheduler::CpuMask::single(target))?;
            wait(|| Ok(scheduler::thread_migration_state(thread)? == (target, None)))?;
            if scheduler::thread_placement(thread)? != (target, scheduler::CpuMask::single(target))
            {
                return Err(Error::AffinityContract);
            }
            // Sample only after handoff completion. A scheduler-field change
            // alone cannot pass: guest instructions must run on the new CPU.
            let previous = counter.load(Ordering::Acquire);
            wait(|| Ok(counter.load(Ordering::Acquire) > previous))?;
            verify_affinity_contract(thread)?;
        }
        // Also exercise stop against one final in-flight migration. Teardown
        // must win regardless of whether its source switch tail completed.
        let (source, _) = scheduler::thread_migration_state(thread)?;
        let target =
            CpuIndex::new(if source.get() == 0 { 1 } else { 0 }).ok_or(Error::InvalidCpu)?;
        scheduler::set_thread_affinity(thread, scheduler::CpuMask::single(target))?;
        Ok(())
    })();
    running.stop();
    super::vm_registry::wait_for_vm_usage_release(&domain).map_err(Error::Fixture)?;
    super::support::quiesce_workers().map_err(Error::Quiescence)?;
    result
}

fn verify_affinity_contract(thread: crate::kernel::task::thread::ThreadId) -> Result<(), Error> {
    use scheduler::{CpuMask, MigrationStatus};

    let (current, pending) = scheduler::thread_migration_state(thread)?;
    if pending.is_some() {
        return Err(Error::AffinityContract);
    }
    let other = CpuIndex::new(if current.get() == 0 { 1 } else { 0 }).ok_or(Error::InvalidCpu)?;
    let both = CpuMask::single(current).with_cpu(other);
    // An admitted current CPU remains assigned when the allowed set grows.
    if scheduler::set_thread_affinity(thread, both)? != MigrationStatus::Completed
        || scheduler::thread_migration_state(thread)? != (current, None)
        || scheduler::thread_placement(thread)? != (current, both)
    {
        return Err(Error::AffinityContract);
    }
    let initial = scheduler::thread_placement(thread)?;
    if scheduler::set_thread_affinity(thread, CpuMask::EMPTY)
        != Err(scheduler::Error::EmptyCpuAffinity)
        || scheduler::thread_placement(thread)? != initial
        || scheduler::thread_migration_state(thread)? != (current, None)
    {
        return Err(Error::AffinityContract);
    }
    // Logical CPU indices are assigned densely during bootstrap admission.
    // Skip only when every representable CPU is online, leaving no invalid set.
    if let Some(offline) = CpuIndex::new(crate::kernel::cpu::online_cpu_count())
        && (scheduler::set_thread_affinity(thread, CpuMask::single(offline))
            != Err(scheduler::Error::NoRegisteredCpuInAffinity)
            || scheduler::thread_placement(thread)? != initial
            || scheduler::thread_migration_state(thread)? != (current, None))
    {
        return Err(Error::AffinityContract);
    }
    Ok(())
}

fn wait(mut ready: impl FnMut() -> Result<bool, Error>) -> Result<(), Error> {
    if crate::kernel::task::wait_for_test_progress(
        crate::kernel::task::TEST_PROGRESS_TIMEOUT_NS,
        &mut ready,
    )? {
        Ok(())
    } else {
        Err(Error::Timeout)
    }
}
