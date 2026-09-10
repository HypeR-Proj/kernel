// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Concurrent raw-register probes across Native traps and scheduler switches.

use crate::kernel::accounting::{ResourceDomain, ResourceLimits};
use crate::kernel::process::{MachineAbi, Process, TaskGroup, TerminalReason};
use crate::kernel::task::scheduler::{self, CpuMask};

use super::{Error, prepare_process, retire_process};

const REGISTER_TIMEOUT_NS: u64 = 20_000_000_000;
static KERNEL_ONLY_WORD: u64 = 0x4859_5045_525f_5553;

pub(crate) fn run() -> Result<(), Error> {
    let direct_calls = crate::hal::user::direct_native_call_count_for_test();
    let domain =
        ResourceDomain::try_new_root(ResourceLimits::UNLIMITED).map_err(|_| Error::Construction)?;
    let group = TaskGroup::try_new(&domain).map_err(|_| Error::Group)?;
    // Sharing one hart forces ownership changes even when several harts are
    // online. The second pass permits independent execution and migration.
    let result = run_pair(&domain, &group, true)
        .and_then(|()| run_pair(&domain, &group, false))
        .and_then(|()| run_fault_probes(&domain, &group));
    group.request_stop().map_err(|_| Error::Group)?;
    group.finish_retirement().map_err(|_| Error::Group)?;
    result?;
    if crate::hal::user::direct_native_call_count_for_test() != direct_calls.saturating_add(256) {
        return Err(Error::Terminal);
    }
    crate::pr_info!("HypeR test: Native register isolation passed (same-hart and migratable)");
    Ok(())
}

fn run_pair(domain: &ResourceDomain, group: &TaskGroup, same_hart: bool) -> Result<(), Error> {
    let program = crate::hal::user::native_register_test_program_for_test();
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
            let cpu = crate::kernel::cpu::current_index().ok_or(Error::Scheduler)?;
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
        let completed = crate::kernel::task::wait_for_test_progress(REGISTER_TIMEOUT_NS, || {
            Ok::<_, Error>(first.try_join().is_some() && second.try_join().is_some())
        })?;
        if !completed {
            return Err(Error::Lifecycle);
        }
        for (process, thread) in [(first, &first_thread), (second, &second_thread)] {
            if process.try_join() != Some(TerminalReason::LastThreadExited { status: 0 })
                || thread.try_join() != Some(TerminalReason::ThreadExited { status: 0 })
            {
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
    let programs = crate::hal::user::native_fault_test_programs_for_test();
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
            let completed =
                crate::kernel::task::wait_for_test_progress(REGISTER_TIMEOUT_NS, || {
                    Ok::<_, Error>(process.try_join().is_some())
                })?;
            if !completed {
                return Err(Error::Lifecycle);
            }
            if process.try_join() != Some(expected) || thread.try_join() != Some(expected) {
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
