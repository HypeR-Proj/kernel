// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Concurrent raw-register probes across Native traps and scheduler switches.

use {crate::accounting::ResourceDomain, crate::accounting::ResourceLimits};
use {
    crate::process::MachineAbi, crate::process::Process, crate::process::TaskGroup,
    crate::process::TerminalReason,
};
use {crate::task::scheduler, crate::task::scheduler::CpuMask};

use super::{Error, prepare_process, retire_process};

const REGISTER_TIMEOUT_NS: u64 = 20_000_000_000;
static KERNEL_ONLY_WORD: u64 = 0x4859_5045_525f_5553;

/// Raw LP64D register-isolation payload, copied into test-owned user memory.
fn register_program() -> &'static [u8] {
    unsafe extern "C" {
        static riscv64_native_register_test_start: u8;
        static riscv64_native_register_test_end: u8;
    }
    let start = core::ptr::addr_of!(riscv64_native_register_test_start);
    let end = core::ptr::addr_of!(riscv64_native_register_test_end);
    // SAFETY: Assembly bounds one immutable, retained position-independent
    // payload. Its bytes remain mapped and unchanged for the machine lifetime.
    unsafe { super::program_bytes(start, end) }
}

/// Containment probes: supervisor-only load through a0, then privileged SATP.
/// A forbidden instruction which incorrectly completes exits with -99, so an
/// unrelated later instruction fault cannot masquerade as successful isolation.
fn fault_programs() -> [&'static [u8]; 2] {
    unsafe extern "C" {
        static riscv64_native_supervisor_load_start: u8;
        static riscv64_native_supervisor_load_end: u8;
        static riscv64_native_privileged_csr_start: u8;
        static riscv64_native_privileged_csr_end: u8;
    }
    [
        (
            &raw const riscv64_native_supervisor_load_start,
            &raw const riscv64_native_supervisor_load_end,
        ),
        (
            &raw const riscv64_native_privileged_csr_start,
            &raw const riscv64_native_privileged_csr_end,
        ),
    ]
    .map(|(start, end)| {
        // SAFETY: The linked test assembly retains each immutable probe in one
        // section between its start/end labels for the entire kernel lifetime.
        unsafe { super::program_bytes(start, end) }
    })
}

pub(crate) fn run() -> Result<(), Error> {
    let direct_calls = crate::hal::user::direct_native_call_count_for_test();
    let domain =
        ResourceDomain::try_new_root(ResourceLimits::UNLIMITED).map_err(|_| Error::Construction)?;
    let group = TaskGroup::try_new(&domain).map_err(|_| Error::Group)?;
    super::verify_thread_creation_affinity(
        &domain,
        &group,
        register_program(),
        MachineAbi::Riscv64,
    )?;
    let result = (|| {
        // Same-hart execution forces ownership changes; the second pass also
        // permits migration. Check the direct-call count before extra probes.
        run_pair(&domain, &group, true)?;
        run_pair(&domain, &group, false)?;
        run_fault_probes(&domain, &group)?;
        // Each of four programs returns from 64 queries, one unknown operation,
        // one invalid target and eight yields; thread_exit never returns.
        if crate::hal::user::direct_native_call_count_for_test() != direct_calls.saturating_add(296)
        {
            return Err(Error::Terminal);
        }
        crate::pr_info!("HypeR test: Native register isolation passed (same-hart and migratable)");
        integer_only(&domain, &group)?;
        super::fp::run(&domain, &group)
    })();
    group.request_stop().map_err(|_| Error::Group)?;
    group.finish_retirement().map_err(|_| Error::Group)?;
    result?;
    Ok(())
}

fn run_pair(domain: &ResourceDomain, group: &TaskGroup, same_hart: bool) -> Result<(), Error> {
    let program = register_program();
    let mut processes: [Option<Process>; 2] = [None, None];
    let result = (|| {
        for slot in &mut processes {
            *slot = Some(prepare_process(
                domain,
                group,
                program,
                MachineAbi::Riscv64,
            )?);
        }
        let [Some(first), Some(second)] = &processes else {
            return Err(Error::Construction);
        };
        let pin = scheduler::preempt_disable().map_err(|_| Error::Scheduler)?;
        let prepared = (|| {
            let cpu = crate::cpu::current_index().ok_or(Error::Scheduler)?;
            let affinity = if same_hart {
                CpuMask::single(cpu)
            } else {
                CpuMask::ALL
            };
            let first_thread = first
                .create_user_thread(
                    "selftest/native-registers-a",
                    first.image().initial_thread().with_argument(0x1000),
                    affinity,
                )
                .map_err(|_| Error::Construction)?;
            let second_thread = second
                .create_user_thread(
                    "selftest/native-registers-b",
                    second.image().initial_thread().with_argument(0x8000),
                    affinity,
                )
                .map_err(|_| Error::Construction)?;
            // Both same-hart contexts become runnable before this controller
            // releases its pin. Distinct seeds expose leaked GP/TP/FP images.
            first_thread.ready().map_err(|_| Error::Scheduler)?;
            second_thread.ready().map_err(|_| Error::Scheduler)?;
            Ok::<_, Error>((first_thread, second_thread))
        })();
        scheduler::preempt_enable_and_reschedule(pin).map_err(|_| Error::Scheduler)?;
        let (first_thread, second_thread) = prepared?;
        let completed = crate::task::wait_for_test_progress(REGISTER_TIMEOUT_NS, || {
            // Process completion precedes the final Thread join notification.
            // On another CPU either process can be stopped while its reaper
            // is still publishing the Thread's completion.
            Ok::<_, Error>(
                first.try_join().is_some()
                    && second.try_join().is_some()
                    && first_thread.try_join().is_some()
                    && second_thread.try_join().is_some(),
            )
        })?;
        if !completed {
            return Err(Error::Lifecycle);
        }
        for (process, thread) in [(first, &first_thread), (second, &second_thread)] {
            let process_exit = process.try_join();
            let thread_exit = thread.try_join();
            if process_exit != Some(TerminalReason::LastThreadExited { status: 0 })
                || thread_exit != Some(TerminalReason::ThreadExited { status: 0 })
            {
                crate::pr_err!(
                    "HypeR test: Native registers same_hart={same_hart}: process={process_exit:?}, thread={thread_exit:?}"
                );
                return Err(Error::Terminal);
            }
            retire_process(process)?;
        }
        Ok(())
    })();
    // Every failure still closes admission for the owned test processes. No
    // worker borrows controller stack data; code and stacks belong to VMOs.
    for process in processes.iter().flatten() {
        stop_and_retire(process)?;
    }
    super::super::support::quiesce_workers().map_err(|_| Error::Lifecycle)?;
    result
}

fn run_fault_probes(domain: &ResourceDomain, group: &TaskGroup) -> Result<(), Error> {
    let programs = fault_programs();
    let expected = [
        TerminalReason::Fault { class: 2, code: 13 },
        TerminalReason::Fault { class: 4, code: 2 },
    ];
    for (program, expected) in programs.into_iter().zip(expected) {
        let process = prepare_process(domain, group, program, MachineAbi::Riscv64)?;
        let result = (|| {
            // The first probe attempts to read a real supervisor mapping, not
            // merely an unmapped address. The static outlives every test Thread.
            let start = process
                .image()
                .initial_thread()
                .with_argument(core::ptr::addr_of!(KERNEL_ONLY_WORD) as u64);
            let thread = process
                .create_user_thread("selftest/native-fault", start, CpuMask::ALL)
                .map_err(|_| Error::Construction)?;
            thread.ready().map_err(|_| Error::Scheduler)?;
            let completed = crate::task::wait_for_test_progress(REGISTER_TIMEOUT_NS, || {
                Ok::<_, Error>(process.try_join().is_some() && thread.try_join().is_some())
            })?;
            if !completed {
                return Err(Error::Lifecycle);
            }
            let process_exit = process.try_join();
            let thread_exit = thread.try_join();
            if process_exit != Some(expected) || thread_exit != Some(expected) {
                crate::pr_err!(
                    "HypeR test: Native fault expected={expected:?}: process={process_exit:?}, thread={thread_exit:?}"
                );
                return Err(Error::Terminal);
            }
            Ok(())
        })();
        stop_and_retire(&process)?;
        super::super::support::quiesce_workers().map_err(|_| Error::Lifecycle)?;
        result?;
    }
    crate::pr_info!("HypeR test: Native supervisor mapping and CSR faults contained");
    Ok(())
}

fn stop_and_retire(process: &Process) -> Result<(), Error> {
    if process.try_join().is_none() {
        let report = process.request_stop(TerminalReason::Requested);
        if !report.dispatch_complete {
            return Err(Error::Lifecycle);
        }
    }
    retire_process(process)
}

fn integer_only(domain: &ResourceDomain, group: &TaskGroup) -> Result<(), Error> {
    let before = crate::hal::user::fp_state_counts_for_test();
    unsafe extern "C" {
        static riscv64_native_integer_exit_start: u8;
        static riscv64_native_integer_exit_end: u8;
    }
    // SAFETY: These labels bound one retained immutable test assembly program.
    // Its invocation exits normally without issuing F/D or illegal instructions.
    let code = unsafe {
        super::program_bytes(
            &raw const riscv64_native_integer_exit_start,
            &raw const riscv64_native_integer_exit_end,
        )
    };
    let process = prepare_process(domain, group, code, MachineAbi::Riscv64)?;
    let result = (|| {
        let thread = process
            .create_initial_user_thread("selftest/integer-only", CpuMask::ALL)
            .map_err(|_| Error::Construction)?;
        thread.ready().map_err(|_| Error::Scheduler)?;
        if !crate::task::wait_for_test_progress(REGISTER_TIMEOUT_NS, || {
            Ok::<_, Error>(process.try_join().is_some())
        })? {
            return Err(Error::Lifecycle);
        }
        if process.try_join() != Some(TerminalReason::LastThreadExited { status: 0 }) {
            return Err(Error::Terminal);
        }
        Ok(())
    })();
    stop_and_retire(&process)?;
    super::super::support::quiesce_workers().map_err(|_| Error::Lifecycle)?;
    result?;
    if crate::hal::user::fp_state_counts_for_test() != before {
        return Err(Error::Terminal);
    }
    crate::pr_info!("HypeR test: integer-only Native run skipped FP restore/save");
    Ok(())
}
