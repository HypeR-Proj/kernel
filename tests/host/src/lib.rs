// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Host-side tests for architecture-independent HypeR mechanisms.
//!
//! Each test module follows a production subsystem or a distinct hardware or
//! lifecycle contract. Keep shared helpers here deliberately small so tests do
//! not acquire a second framework of their own.

extern crate alloc;

#[cfg(test)]
#[path = "../../../src/arch/aarch64/stage2_retirement.rs"]
mod aarch64_stage2_retirement_model;
#[cfg(test)]
#[path = "../../../src/arch/aarch64/user_contract.rs"]
mod aarch64_user_contract_model;
#[cfg(test)]
#[path = "../../../src/arch/aarch64/registers.rs"]
mod registers;
#[cfg(test)]
#[path = "../../../src/kernel/vm/address_space_state.rs"]
mod vm_address_space_state_model;
#[cfg(test)]
#[path = "../../../src/kernel/vm/endpoint_state.rs"]
mod vm_endpoint_state_model;
#[cfg(test)]
#[path = "../../../src/kernel/vm/endpoint_wait.rs"]
mod vm_endpoint_wait_model;
#[cfg(test)]
#[path = "../../../src/kernel/vm/residency_state.rs"]
mod vm_residency_state_model;

#[cfg(test)]
fn require_ok<T, E: core::fmt::Debug>(result: Result<T, E>) -> T {
    match result {
        Ok(value) => value,
        Err(error) => panic!("required success, received {error:?}"),
    }
}

#[cfg(test)]
fn require_some<T>(value: Option<T>) -> T {
    match value {
        Some(value) => value,
        None => panic!("required a value"),
    }
}

#[cfg(test)]
#[path = "cases/aarch64_cache.rs"]
mod aarch64_cache;
#[cfg(test)]
#[path = "cases/aarch64_user_contract.rs"]
mod aarch64_user_contract;
#[cfg(test)]
#[path = "cases/address_space_residency.rs"]
mod address_space_residency;
#[cfg(test)]
#[path = "cases/allocation.rs"]
mod allocation;
#[cfg(test)]
#[path = "cases/allocator_local_cache.rs"]
mod allocator_local_cache;
#[cfg(test)]
#[path = "cases/boot_allocator.rs"]
mod boot_allocator;
#[cfg(test)]
#[path = "cases/cache_publication.rs"]
mod cache_publication;
#[cfg(test)]
#[path = "cases/capability_core.rs"]
mod capability_core;
#[cfg(test)]
#[path = "cases/cpio.rs"]
mod cpio;
#[cfg(test)]
#[path = "cases/cpu.rs"]
mod cpu;
#[cfg(test)]
#[path = "cases/crash_supplement.rs"]
mod crash_supplement;
#[cfg(test)]
#[path = "cases/cross_call_pinning.rs"]
mod cross_call_pinning;
#[cfg(test)]
#[path = "cases/elf.rs"]
mod elf;
#[cfg(test)]
#[path = "cases/exec_startup.rs"]
mod exec_startup;
#[cfg(test)]
#[path = "cases/fallible_ownership.rs"]
mod fallible_ownership;
#[cfg(test)]
#[path = "cases/fdt.rs"]
mod fdt;
#[cfg(test)]
#[path = "cases/foreign_memory_copy.rs"]
mod foreign_memory_copy;
#[cfg(test)]
#[path = "cases/generic_timer.rs"]
mod generic_timer;
#[cfg(test)]
#[path = "cases/gicv3.rs"]
mod gicv3;
#[cfg(test)]
#[path = "cases/guest_gicv3.rs"]
mod guest_gicv3;
#[cfg(test)]
#[path = "cases/kallsyms.rs"]
mod kallsyms;
#[cfg(test)]
#[path = "cases/kaslr.rs"]
mod kaslr;
#[cfg(test)]
#[path = "cases/kernel_log.rs"]
mod kernel_log;
#[cfg(test)]
#[path = "cases/mmio_resources.rs"]
mod mmio_resources;
#[cfg(test)]
#[path = "cases/native_abi.rs"]
mod native_abi;
#[cfg(test)]
#[path = "cases/ns16550.rs"]
mod ns16550;
#[cfg(test)]
#[path = "cases/physical_ranges.rs"]
mod physical_ranges;
#[cfg(test)]
#[path = "cases/plic.rs"]
mod plic;
#[cfg(test)]
#[path = "cases/process_lifecycle.rs"]
mod process_lifecycle;
#[cfg(test)]
#[path = "cases/psci.rs"]
mod psci;
#[cfg(test)]
#[path = "cases/resource_domain.rs"]
mod resource_domain;
#[cfg(test)]
#[path = "cases/runtime_allocators.rs"]
mod runtime_allocators;
#[cfg(test)]
#[path = "cases/scheduler_policy.rs"]
mod scheduler_policy;
#[cfg(test)]
#[path = "cases/scheduler_requests.rs"]
mod scheduler_requests;
#[cfg(test)]
#[path = "cases/scheduler_residence.rs"]
mod scheduler_residence;
#[cfg(test)]
#[path = "cases/slab_partial.rs"]
mod slab_partial;
#[cfg(test)]
#[path = "cases/software_timers.rs"]
mod software_timers;
#[cfg(test)]
#[path = "cases/stage2_mapping.rs"]
mod stage2_mapping;
#[cfg(test)]
#[path = "cases/synchronization.rs"]
mod synchronization;
#[cfg(test)]
#[path = "cases/translation_id.rs"]
mod translation_id;
#[cfg(test)]
#[path = "cases/user_memory.rs"]
mod user_memory;
#[cfg(test)]
#[path = "cases/vgic.rs"]
mod vgic;
#[cfg(test)]
#[path = "cases/virtual_legacy_pc.rs"]
mod virtual_legacy_pc;
#[cfg(test)]
#[path = "cases/virtual_pl011.rs"]
mod virtual_pl011;
#[cfg(test)]
#[path = "cases/vm_interrupt_reconcile.rs"]
mod vm_interrupt_reconcile;
#[cfg(test)]
#[path = "cases/vm_mmio_diagnostics.rs"]
mod vm_mmio_diagnostics;
#[cfg(test)]
#[path = "cases/vm_run_admission.rs"]
mod vm_run_admission;
#[cfg(test)]
#[path = "cases/x86_svm_contract.rs"]
mod x86_svm_contract;
#[cfg(test)]
#[path = "cases/x86_virtual_cpu_contract.rs"]
mod x86_virtual_cpu_contract;
