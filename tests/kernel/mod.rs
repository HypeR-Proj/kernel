// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Bare-metal integration tests compiled only by `kernel-self-test`.

mod capability_objects;
mod channel;
#[cfg(CONFIG_ARCH_AARCH64)]
mod channel_service;
#[cfg(CONFIG_ARCH_AARCH64)]
mod guest_entry_irq;
mod guest_memory_access;
#[cfg(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64))]
mod irq_tail_preemption;
mod log_flush_barrier;
mod native_syscall;
#[cfg(CONFIG_ARCH_AARCH64)]
mod native_user_entry;
mod object_directory;
mod object_wait;
#[cfg(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_X86_64))]
mod reschedule_ipi;
mod scheduler_sync;
mod stack_model;
mod startup_readiness;
mod support;
mod thread_migration;
mod thread_sleep;
#[cfg(CONFIG_ARCH_AARCH64)]
mod user_memory_access;
mod vm_registry;
mod vm_wfi_wait;
mod wait_arbitration;

pub(crate) fn run() {
    crate::hal::irq::enable_local();
    #[cfg(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_X86_64))]
    run_case("reschedule IPI runtime proof", reschedule_ipi::run);
    run_case("kernel scheduler/sync tests", scheduler_sync::run);
    run_case(
        "kernel log flush-barrier cancellation tests",
        log_flush_barrier::run,
    );
    run_case("kernel stack-model tests", stack_model::run);
    run_case("kernel thread-sleep tests", thread_sleep::run);
    run_case("kernel thread-migration tests", thread_migration::run);
    #[cfg(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64))]
    run_case(
        "guest IRQ-tail preemption setup",
        irq_tail_preemption::install,
    );
    run_case(
        "kernel Native syscall validation tests",
        native_syscall::run,
    );
    #[cfg(CONFIG_ARCH_AARCH64)]
    run_case("AArch64 native-user entry tests", native_user_entry::run);
    run_case("kernel object-wait tests", object_wait::run);
    run_case("kernel object-directory tests", object_directory::run);
    run_case("kernel Channel core tests", channel::run);
    run_case(
        "kernel init capability-object tests",
        capability_objects::run,
    );
    #[cfg(CONFIG_ARCH_AARCH64)]
    run_case(
        "kernel Channel service transaction tests",
        channel_service::run,
    );
    run_case("kernel wait-arbitration tests", wait_arbitration::run);
    run_case("kernel startup-readiness tests", startup_readiness::run);
    let guest_execution = crate::hal::vm::guest_execution_available();
    if guest_execution {
        run_case("kernel guest-memory access tests", guest_memory_access::run);
    }
    #[cfg(CONFIG_ARCH_AARCH64)]
    run_case(
        "kernel application-memory access tests",
        user_memory_access::run,
    );
    run_case("kernel VM registry tests", vm_registry::run);
    run_case("kernel vCPU endpoint wait tests", vm_wfi_wait::run);
    crate::hal::irq::mask_local();
    #[cfg(CONFIG_ARCH_AARCH64)]
    run_case(
        "AArch64 guest-entry IRQ mask contract",
        guest_entry_irq::run,
    );
    crate::hal::irq::enable_local();
    crate::pr_info!("HypeR test: scheduler ready/wait queues and sleeping sync passed");
    crate::pr_info!("HypeR test: guarded thread, IRQ, and emergency stacks passed");
    crate::pr_info!("HypeR test: deadline-based thread sleep passed");
    crate::pr_info!("HypeR test: race-safe wait arbitration passed");
    crate::pr_info!("HypeR test: level-triggered object waits passed");
    crate::pr_info!("HypeR test: bounded Channel FIFO and signal lifecycle passed");
    crate::pr_info!("HypeR test: typed init capability objects passed");
    #[cfg(CONFIG_ARCH_AARCH64)]
    crate::pr_info!("HypeR test: Channel Process and user-copy transactions passed");
    crate::pr_info!("HypeR test: Native syscall validation passed");
    #[cfg(CONFIG_ARCH_AARCH64)]
    crate::pr_info!("HypeR test: AArch64 EL0 syscall and fault containment passed");
    crate::pr_info!("HypeR test: fatal-path readiness contract passed");
    #[cfg(CONFIG_ARCH_AARCH64)]
    crate::pr_info!("HypeR test: AArch64 guest-entry IRQ mask contract passed");
    if guest_execution {
        crate::pr_info!("HypeR test: checked stage-2 guest-memory copies passed");
    } else {
        crate::pr_notice!("HypeR test: stage-2 guest-memory copies skipped (no virtualization)");
    }
    crate::pr_info!("HypeR test: checked application-memory copies passed");
    if guest_execution {
        crate::pr_info!("HypeR test: VM and dormant-vCPU rollback passed");
    } else {
        crate::pr_info!("HypeR test: VM reservation rollback passed");
        crate::pr_notice!("HypeR test: dormant-vCPU rollback skipped (no virtualization)");
    }
}

fn run_case<E: core::fmt::Debug>(operation: &'static str, test: impl FnOnce() -> Result<(), E>) {
    if let Err(error) = test() {
        crate::kernel::boot::fail(operation, error);
    }
}
