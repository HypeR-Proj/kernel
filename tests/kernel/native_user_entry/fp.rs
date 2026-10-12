// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Real lower-world register isolation, scheduling, and migration probes.

use super::{Error, IMAGE_BASE, prepare_process, retire_process};
use crate::kernel::accounting::ResourceDomain;
use crate::kernel::mm::user_space::{UserAddress, UserSlice};
use crate::kernel::process::{MachineAbi, Process, TaskGroup, TerminalReason, UserThread};
use crate::kernel::task::{self, scheduler};
use hyper::{cpu::CpuIndex, mm::PAGE_SIZE};

unsafe extern "C" {
    #[cfg_attr(CONFIG_ARCH_AARCH64, link_name = "aarch64_native_fp_probe_start")]
    #[cfg_attr(CONFIG_ARCH_RISCV64, link_name = "riscv64_native_fp_probe_start")]
    static native_fp_probe_start: u8;
    #[cfg_attr(CONFIG_ARCH_AARCH64, link_name = "aarch64_native_fp_probe_end")]
    #[cfg_attr(CONFIG_ARCH_RISCV64, link_name = "riscv64_native_fp_probe_end")]
    static native_fp_probe_end: u8;
    #[cfg_attr(CONFIG_ARCH_AARCH64, link_name = "aarch64_guest_fp_probe_start")]
    #[cfg_attr(CONFIG_ARCH_RISCV64, link_name = "riscv64_guest_fp_probe_start")]
    static guest_fp_probe_start: u8;
    #[cfg_attr(CONFIG_ARCH_AARCH64, link_name = "aarch64_guest_fp_probe_end")]
    #[cfg_attr(CONFIG_ARCH_RISCV64, link_name = "riscv64_guest_fp_probe_end")]
    static guest_fp_probe_end: u8;
    #[cfg_attr(CONFIG_ARCH_AARCH64, link_name = "aarch64_guest_fp_wait_probe_start")]
    #[cfg_attr(CONFIG_ARCH_RISCV64, link_name = "riscv64_guest_fp_wait_probe_start")]
    static guest_fp_wait_probe_start: u8;
    #[cfg_attr(CONFIG_ARCH_AARCH64, link_name = "aarch64_guest_fp_wait_probe_end")]
    #[cfg_attr(CONFIG_ARCH_RISCV64, link_name = "riscv64_guest_fp_wait_probe_end")]
    static guest_fp_wait_probe_end: u8;
}

fn program(start: *const u8, end: *const u8) -> &'static [u8] {
    let length = (end as usize).saturating_sub(start as usize);
    // SAFETY: Callers provide boundaries of one immutable linked assembly
    // payload. The linker keeps both labels in the same allocated section.
    unsafe { core::slice::from_raw_parts(start, length) }
}

pub(crate) fn guest_program(wait: bool) -> &'static [u8] {
    if wait {
        program(
            &raw const guest_fp_wait_probe_start,
            &raw const guest_fp_wait_probe_end,
        )
    } else {
        program(
            &raw const guest_fp_probe_start,
            &raw const guest_fp_probe_end,
        )
    }
}

struct Probe {
    process: Process,
    thread: UserThread,
}

impl Probe {
    fn progress(&self) -> Result<u64, Error> {
        if self.thread.snapshot().terminal.is_some() {
            return Err(Error::Terminal);
        }
        let source = UserSlice::new(UserAddress::new(IMAGE_BASE + 2 * PAGE_SIZE), 8)
            .map_err(|_| Error::Construction)?;
        let mut value = [0; 8];
        self.process
            .copy_from_user(source, &mut value)
            .map_err(|_| Error::AddressSpace)?;
        let value = u64::from_le_bytes(value);
        if value == u64::MAX {
            return Err(Error::Terminal);
        }
        Ok(value)
    }

    fn on_cpu(&self, cpu: CpuIndex) -> Result<bool, Error> {
        let thread = self.thread.scheduler_id().ok_or(Error::Scheduler)?;
        Ok(scheduler::thread_migration_state(thread).map_err(|_| Error::Scheduler)? == (cpu, None))
    }

    fn move_to(&self, cpu: CpuIndex) -> Result<(), Error> {
        let thread = self.thread.scheduler_id().ok_or(Error::Scheduler)?;
        scheduler::set_thread_affinity(thread, scheduler::CpuMask::single(cpu))
            .map_err(|_| Error::Scheduler)?;
        Ok(())
    }
}

fn with_probe(
    domain: &ResourceDomain,
    group: &TaskGroup,
    seed: u16,
    test: impl FnOnce(&Probe) -> Result<(), Error>,
) -> Result<(), Error> {
    let source = program(
        &raw const native_fp_probe_start,
        &raw const native_fp_probe_end,
    );
    let mut code = [0u8; PAGE_SIZE as usize];
    if source.len() > code.len() || source.len() < 4 {
        return Err(Error::Image);
    }
    code[..source.len()].copy_from_slice(source);
    // Each independent owner uses a different complete bank.
    #[cfg(CONFIG_ARCH_AARCH64)]
    let abi = {
        // MOVZ x19, #seed.
        code[..4].copy_from_slice(&(0xd2800013u32 | (u32::from(seed) << 5)).to_le_bytes());
        MachineAbi::Aarch64
    };
    #[cfg(CONFIG_ARCH_RISCV64)]
    let abi = {
        // Noncompressed LUI/ADDI s3 form a distinct positive 16-bit seed.
        let high = (u32::from(seed) + 0x800) >> 12;
        let low = u32::from(seed) & 0xfff;
        code[..4].copy_from_slice(&(0x9b7u32 | (high << 12)).to_le_bytes());
        code[4..8].copy_from_slice(&(0x98993u32 | (low << 20)).to_le_bytes());
        MachineAbi::Riscv64
    };
    let process = prepare_process(domain, group, &code[..source.len()], abi)?;
    let result = (|| {
        let cpu = CpuIndex::new(0).ok_or(Error::Scheduler)?;
        let thread = process
            .create_initial_user_thread("selftest/fp-owner", scheduler::CpuMask::single(cpu))
            .map_err(|_| Error::Construction)?;
        thread.ready().map_err(|_| Error::Scheduler)?;
        test(&Probe {
            process: process.clone(),
            thread,
        })
    })();
    process.request_stop(TerminalReason::Requested);
    retire_process(&process)?;
    result
}

fn wait(mut condition: impl FnMut() -> Result<bool, Error>) -> Result<(), Error> {
    if task::wait_for_test_progress(task::TEST_PROGRESS_TIMEOUT_NS, &mut condition)? {
        Ok(())
    } else {
        Err(Error::Lifecycle)
    }
}

fn with_guest(
    test: impl FnOnce(&core::sync::atomic::AtomicU64) -> Result<(), Error>,
) -> Result<(), Error> {
    let (prepared, domain, counter) =
        super::super::vm_registry::prepare_migration_guest(guest_program(false))
            .map_err(|_| Error::Construction)?;
    let installed = prepared.install().map_err(|_| Error::Construction)?;
    let running = installed
        .start_boot_for_test()
        .map_err(|_| Error::Scheduler)?;
    // SAFETY: The installed guest retains its initialized, aligned RAM until
    // stop completes below. Its release counter store pairs with our atomic loads.
    let result = test(unsafe { &*counter });
    running.stop();
    super::super::vm_registry::wait_for_vm_usage_release(&domain).map_err(|_| Error::Lifecycle)?;
    result
}

fn guest_progress(counter: &core::sync::atomic::AtomicU64) -> Result<u64, Error> {
    let value = counter.load(core::sync::atomic::Ordering::Acquire);
    if value == u64::MAX {
        return Err(Error::Terminal);
    }
    Ok(value)
}

pub(super) fn run(domain: &ResourceDomain, group: &TaskGroup) -> Result<(), Error> {
    let before = crate::hal::user::fp_state_counts_for_test();
    let result = with_probe(domain, group, 0x1235, |first| {
        with_probe(domain, group, 0x678a, |second| {
            with_guest(|guest| {
                wait(|| {
                    Ok(first.progress()? > 0
                        && second.progress()? > 0
                        && guest_progress(guest)? > 0)
                })?;
                for round in 0..8 {
                    // Two Native owners share each selected CPU with the busy
                    // guest on CPU 0, including preemption and live migration.
                    let index = if crate::kernel::cpu::online_cpu_count() > 1 {
                        round % 2
                    } else {
                        0
                    };
                    let cpu = CpuIndex::new(index).ok_or(Error::Scheduler)?;
                    first.move_to(cpu)?;
                    second.move_to(cpu)?;
                    wait(|| Ok(first.on_cpu(cpu)? && second.on_cpu(cpu)?))?;
                    let a = first.progress()?;
                    let b = second.progress()?;
                    let c = guest_progress(guest)?;
                    wait(|| {
                        Ok(first.progress()? > a
                            && second.progress()? > b
                            && guest_progress(guest)? > c)
                    })?;
                }
                Ok(())
            })
        })
    });
    super::super::support::quiesce_workers().map_err(|_| Error::Scheduler)?;
    result?;
    let after = crate::hal::user::fp_state_counts_for_test();
    if after.0 <= before.0 || after.1 <= before.1 || after.0 - before.0 != after.1 - before.1 {
        return Err(Error::Terminal);
    }
    crate::pr_info!("HypeR test: Native lazy FP and guest register isolation passed");
    Ok(())
}
