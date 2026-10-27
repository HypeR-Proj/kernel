// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use super::services::ProcessServices;
use crate::abi::native::TaskServices;
use crate::task::scheduler::CpuMask;
use {crate::capability::HandleValue, crate::capability::Rights};
use {crate::mm::user_space::UserAddress, crate::mm::user_space::UserSlice};
use {crate::process::Process, crate::process::UserThread, crate::process::UserThreadPhase};

/// Exercises the real thread service before the caller becomes runnable.
pub(crate) fn verify_thread_affinity_creation_for_test(
    process: &Process,
    caller: &UserThread,
    mask_address: UserAddress,
) -> Result<(), &'static str> {
    const WORDS: usize = hyper::abi::native::HYPER_NATIVE_PROCESS_AFFINITY_MAX_WORDS as usize;
    const BYTES: usize = WORDS * core::mem::size_of::<u64>();
    let services = ProcessServices::new(process, caller);
    let caller_id = caller.scheduler_id().ok_or("caller scheduler identity")?;
    let (cpu, _) =
        crate::task::scheduler::thread_placement(caller_id).map_err(|_| "caller placement")?;
    let inherited = CpuMask::single(cpu);
    crate::task::scheduler::set_thread_affinity(caller_id, inherited)
        .map_err(|_| "caller affinity update")?;
    let original =
        crate::task::scheduler::scheduling_snapshot(caller_id).map_err(|_| "caller scheduling")?;
    use {
        crate::task::policy::SchedulingParameters, crate::task::policy::ThreadPriority,
        crate::task::policy::ThreadWeight,
    };
    use {
        crate::task::scheduling_authority::SCHEDULING_RIGHTS,
        crate::task::scheduling_authority::SchedulingAuthority,
        crate::task::scheduling_authority::SchedulingLimits,
    };
    let lowered = SchedulingParameters::fair(
        ThreadWeight::new(1).ok_or("test weight")?,
        original.quantum_ns(),
        ThreadPriority::LOWEST,
        original.role(),
        original.fair_backend(),
    )
    .ok_or("test scheduling")?;
    crate::task::scheduler::configure_scheduling(
        caller_id,
        lowered,
        SchedulingAuthority::new(Rights::SCHED_LOWER),
        SchedulingLimits::inherited(original),
    )
    .map_err(|_| "caller scheduling demotion")?;
    let image = process.image();
    let entry = image.initial_thread();
    // The test passes actual mapped user addresses; creation itself must not
    // execute the child or consume its stack before the explicit start syscall.
    let create = |input, count| {
        services.create_thread(
            Rights::SCHED_LOWER,
            entry.entry().get(),
            entry.stack().get(),
            0,
            23,
            input,
            count,
        )
    };
    let check = |handle: HandleValue, expected: CpuMask| -> Result<(), &'static str> {
        let receiver = process
            .resolve_invocation(handle)
            .map_err(|_| "created thread receiver")?;
        let thread = UserThread::resolve_receiver(&receiver, Rights::INSPECT)
            .map_err(|_| "created thread resolve")?;
        let id = thread.scheduler_id().ok_or("created scheduler identity")?;
        let actual_scheduling =
            crate::task::scheduler::scheduling_snapshot(id).map_err(|_| "created scheduling")?;
        if actual_scheduling != lowered
            || receiver.info().rights.intersection(SCHEDULING_RIGHTS) != Rights::SCHED_LOWER
        {
            return Err("thread creation restored scheduling or elevated rights");
        }
        let (_, actual) =
            crate::task::scheduler::thread_placement(id).map_err(|_| "created placement")?;
        if actual != expected || thread.snapshot().phase != UserThreadPhase::Dormant {
            return Err("thread creation affinity or dormant state");
        }
        process
            .close_handle(handle)
            .map_err(|_| "close dormant child")?;
        thread.join().map_err(|_| "dormant child cancellation")?;
        Ok(())
    };
    check(
        create(None, 0).map_err(|_| "inherited creation")?,
        inherited,
    )?;
    let selected = (0..crate::cpu::online_cpu_count())
        .filter_map(hyper::cpu::CpuIndex::new)
        .find(|other| *other != cpu)
        .unwrap_or(cpu);
    let explicit = CpuMask::single(selected);
    let input = UserSlice::new(mask_address, BYTES as u64).map_err(|_| "mask slice")?;
    let write = |cpu: Option<usize>| {
        let mut bytes = [0u8; BYTES];
        if let Some(cpu) = cpu {
            bytes[cpu / 8] |= 1 << (cpu % 8);
        }
        process
            .copy_to_user(input, &bytes)
            .map_err(|_| "mask write")
    };
    write(Some(selected.get()))?;
    check(
        create(Some(input), WORDS).map_err(|_| "explicit creation")?,
        explicit,
    )?;
    let before = process.snapshot();
    write(None)?;
    if create(Some(input), WORDS).is_ok() {
        return Err("empty affinity accepted");
    }
    if crate::cpu::online_cpu_count() < WORDS * u64::BITS as usize {
        write(Some(crate::cpu::online_cpu_count()))?;
        if create(Some(input), WORDS).is_ok() {
            return Err("unavailable affinity accepted");
        }
    }
    let bad = UserSlice::new(UserAddress::new(0x1000), 8).map_err(|_| "bad mask slice")?;
    if create(Some(bad), 1).is_ok() {
        return Err("unmapped affinity accepted");
    }
    let after = process.snapshot();
    if after.active_threads != before.active_threads
        || after.pending_threads != before.pending_threads
    {
        return Err("failed affinity creation published thread");
    }
    crate::task::scheduler::configure_scheduling(
        caller_id,
        original,
        SchedulingAuthority::new(SCHEDULING_RIGHTS),
        SchedulingLimits::SYSTEM,
    )
    .map_err(|_| "restore caller scheduling")?;
    Ok(())
}
