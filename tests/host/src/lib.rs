// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Host-side tests for architecture-independent HypeR mechanisms.
//!
//! Each test module follows a production subsystem or a distinct hardware or
//! lifecycle contract. Keep shared helpers here deliberately small so tests do
//! not acquire a second framework of their own.

extern crate alloc;

#[cfg(test)]
#[path = "cases/trace.rs"]
mod trace;
#[cfg(test)]
#[path = "cases/trace_macros.rs"]
mod trace_macros;

#[cfg(test)]
#[path = "cases/table_edit.rs"]
mod table_edit;

#[cfg(test)]
#[path = "../../../src/block/transfer.rs"]
mod block_transfer;
#[cfg(test)]
#[path = "cases/block_transfers.rs"]
mod block_transfers;
#[cfg(test)]
#[path = "cases/smmuv3.rs"]
mod smmuv3;

#[cfg(test)]
#[path = "../../../core/src/mm/allocation/refcount.rs"]
mod fallible_refcount;

#[cfg(all(test, target_arch = "aarch64"))]
#[path = "../../../hal/src/arch/aarch64/user/copy.rs"]
mod aarch64_exposed_copy;
#[cfg(all(test, target_arch = "aarch64"))]
#[path = "cases/aarch64_exposed_copy.rs"]
mod aarch64_exposed_copy_tests;

#[cfg(test)]
#[path = "cases/riscv_devices.rs"]
mod riscv_devices;

#[cfg(test)]
#[path = "../../../hal/src/arch/riscv64/memory/translation_id_policy.rs"]
mod riscv_translation_id_policy;

#[cfg(test)]
#[path = "cases/riscv_user_contract.rs"]
mod riscv_user_contract;
#[cfg(test)]
#[path = "../../../hal/src/arch/riscv64/user/contract.rs"]
mod riscv_user_contract_model;

#[cfg(test)]
#[path = "cases/sharded_lock.rs"]
mod sharded_lock;

#[cfg(test)]
#[path = "../../../hal/src/arch/aarch64/memory/address_layout.rs"]
mod aarch64_address_layout_model;
#[cfg(test)]
#[path = "../../../hal/src/arch/aarch64/vm/cpu_contract.rs"]
mod aarch64_guest_cpu_contract_model;
#[cfg(test)]
#[path = "../../../hal/src/arch/aarch64/user/contract.rs"]
mod aarch64_user_contract_model;
#[cfg(test)]
#[path = "../../../src/fs/cache/worker/request_state.rs"]
mod cache_reclaim_requests;
#[cfg(test)]
#[path = "../../../src/task/external_execution.rs"]
mod external_execution_model;
#[cfg(test)]
#[path = "../../../src/fs/cache/mod.rs"]
mod file_data_cache;
#[cfg(test)]
#[path = "cases/filesystem_cache.rs"]
mod filesystem_cache_cases;
#[cfg(test)]
#[path = "../../../hal/src/arch/aarch64/registers.rs"]
mod registers;
#[cfg(test)]
#[path = "../../../src/fs/vfs/read_contract.rs"]
mod vfs_read_contract;
#[cfg(test)]
#[path = "../../../src/fs/vfs/resolve_state.rs"]
mod vfs_resolve_state;
#[cfg(test)]
#[path = "../../../src/fs/vfs/rights_contract.rs"]
mod vfs_rights_contract;
#[cfg(test)]
#[path = "../../../src/vm/address_space_state.rs"]
mod vm_address_space_state_model;
#[cfg(test)]
#[path = "../../../src/vm/endpoint_state.rs"]
mod vm_endpoint_state_model;
#[cfg(test)]
#[path = "../../../src/vm/endpoint_wait.rs"]
mod vm_endpoint_wait_model;
#[cfg(test)]
#[path = "../../../src/vm/residency_state.rs"]
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
#[path = "cases/aarch64_guest_cpu_contract.rs"]
mod aarch64_guest_cpu_contract;
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
#[path = "cases/assigned_device.rs"]
mod assigned_device;
#[cfg(test)]
#[path = "cases/boot_allocator.rs"]
mod boot_allocator;
#[cfg(test)]
#[path = "cases/bootstrap_elf.rs"]
mod bootstrap_elf;
#[cfg(test)]
#[path = "cases/cache_publication.rs"]
mod cache_publication;
#[cfg(test)]
#[path = "cases/capability_core.rs"]
mod capability_core;
#[cfg(test)]
#[path = "../../../src/ipc/capability_wire.rs"]
mod capability_wire;
#[cfg(test)]
#[path = "cases/cpio.rs"]
mod cpio;
#[cfg(test)]
#[path = "cases/cpu.rs"]
mod cpu;
#[cfg(test)]
#[path = "cases/crash_text.rs"]
mod crash_text;
#[cfg(test)]
#[path = "cases/fallible_ownership.rs"]
mod fallible_ownership;
#[cfg(test)]
#[path = "cases/fdt.rs"]
mod fdt;
#[cfg(test)]
#[path = "cases/file_cache_read.rs"]
mod file_cache_read;
#[cfg(test)]
#[path = "cases/file_data_cache.rs"]
mod file_data_cache_cases;
#[cfg(test)]
#[path = "cases/file_read_ahead.rs"]
mod file_read_ahead;
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
#[path = "cases/guest_gic.rs"]
mod guest_gic;
#[cfg(test)]
#[path = "cases/guest_psci.rs"]
mod guest_psci;
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
#[path = "cases/pci/mod.rs"]
mod pci;
#[cfg(test)]
#[path = "cases/physical_ranges.rs"]
mod physical_ranges;
#[cfg(test)]
#[path = "cases/plic.rs"]
mod plic;
#[cfg(test)]
#[path = "cases/process_builder_input.rs"]
mod process_builder_input;
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
mod runtime_allocator_tests;
#[cfg(test)]
#[path = "cases/sbsa_uart.rs"]
mod sbsa_uart;
#[cfg(test)]
#[path = "cases/scheduler_policy.rs"]
mod scheduler_policy;
#[cfg(test)]
#[path = "../../../src/task/policy.rs"]
mod scheduler_policy_model;

#[cfg(test)]
#[path = "../../../src/vm/timer/recovery.rs"]
mod guest_timer_recovery;
#[cfg(test)]
#[path = "cases/scheduler_algorithms.rs"]
mod scheduler_algorithms;
#[cfg(test)]
#[path = "../../../src/time/deadline_slot.rs"]
mod scheduler_deadline_slot;
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
#[path = "cases/timer_tree.rs"]
mod timer_tree;
#[cfg(test)]
#[path = "cases/timestamps.rs"]
mod timestamps;
#[cfg(test)]
#[path = "cases/user_memory.rs"]
mod user_memory;
#[cfg(test)]
#[path = "cases/user_mmio.rs"]
mod user_mmio;
#[cfg(test)]
#[path = "cases/vfs_core.rs"]
mod vfs_core;

#[cfg(test)]
#[path = "cases/file_locks.rs"]
mod file_locks;
#[cfg(test)]
#[path = "cases/vfs_read_contract.rs"]
mod vfs_read_contract_cases;
#[cfg(test)]
#[path = "cases/vfs_rights_contract.rs"]
mod vfs_rights_contract_cases;
#[cfg(test)]
#[path = "cases/vfs_scratch.rs"]
mod vfs_scratch;
#[cfg(test)]
#[path = "cases/vgic.rs"]
mod vgic;

#[cfg(test)]
#[path = "cases/vgic_quiesce.rs"]
mod vgic_quiesce;
#[cfg(test)]
#[path = "cases/virtual_legacy_pc.rs"]
mod virtual_legacy_pc;
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

#[cfg(test)]
#[path = "cases/serial_ring.rs"]
mod serial_ring;

#[cfg(test)]
#[path = "cases/file_data.rs"]
mod file_data;

#[cfg(test)]
#[path = "cases/riscv_guest_mechanisms.rs"]
mod riscv_guest_mechanisms;

#[cfg(test)]
#[path = "../../../hal/src/arch/riscv64/isa.rs"]
mod riscv_isa_contract;

#[cfg(test)]
#[path = "cases/gicv2.rs"]
mod gicv2;

#[cfg(test)]
#[path = "../../../hal/src/arch/aarch64/memory/bootstrap_map.rs"]
mod bootstrap_map;
#[cfg(test)]
#[path = "cases/bootstrap_map.rs"]
mod bootstrap_map_tests;

#[cfg(test)]
#[path = "cases/guest_io.rs"]
mod guest_io;

#[cfg(test)]
#[path = "../../../src/block/wire.rs"]
mod native_block_wire;

#[cfg(test)]
#[path = "../../../src/vm/memory/extent_index.rs"]
mod guest_extent_index;

#[cfg(test)]
#[path = "../../../src/vm/memory/grant_state.rs"]
mod guest_grant_state;

#[cfg(test)]
#[path = "../../../src/vm/retirement_observation.rs"]
mod vm_retirement_observation;

#[cfg(test)]
#[path = "../../../src/task/scheduler/switch_handoff.rs"]
mod scheduler_switch_handoff;

#[cfg(test)]
#[path = "../../../hal/src/arch/aarch64/memory/translation_identifiers.rs"]
mod aarch64_translation_identifiers;

#[cfg(test)]
#[path = "../../../core/src/mm/translation_epoch.rs"]
mod translation_epoch_model;

#[cfg(test)]
#[path = "../../../hal/src/arch/aarch64/vm/vgic/common.rs"]
mod vgic_common;

#[cfg(test)]
#[path = "../../../hal/src/arch/aarch64/vm/vgic/bank.rs"]
mod vgic_bank;

#[cfg(test)]
#[path = "../../../core/src/collections/persistent_avl.rs"]
mod persistent_avl;

#[cfg(test)]
#[path = "../../../core/src/collections/indexed_heap.rs"]
mod indexed_heap;

#[cfg(test)]
#[path = "cases/collection_links.rs"]
mod collection_links;

#[cfg(test)]
#[path = "cases/random.rs"]
mod random;

#[cfg(test)]
#[path = "cases/scheduling_authority.rs"]
mod scheduling_authority;

#[cfg(test)]
#[path = "../../../src/mm/allocator/buddy.rs"]
pub mod runtime_buddy;
#[cfg(test)]
#[path = "../../../src/mm/allocator/heap/mod.rs"]
pub mod runtime_heap;
#[cfg(test)]
pub mod runtime_allocators {
    pub use crate::runtime_buddy::{BuddyAllocator, BuddyError, BuddyStats, MAX_ORDER};
    pub use crate::runtime_heap as heap;
    pub use hyper::mm::boot::MemoryHandoff;
}
#[cfg(test)]
pub mod mm {
    pub use crate::runtime_allocators as allocator;
    pub use hyper::mm::*;
}

#[cfg(test)]
#[path = "../../../src/fs/primitives/mod.rs"]
pub mod filesystem_primitives;

#[cfg(test)]
#[path = "../../../src/fs/archive/mod.rs"]
pub mod filesystem_archive;

#[cfg(test)]
#[path = "../../../src/diagnostics/log/storage/mod.rs"]
pub mod log_storage;

#[cfg(test)]
#[path = "../../../src/time/queue/mod.rs"]
pub mod time_queue;

#[cfg(test)]
#[path = "../../../src/process/formats/mod.rs"]
pub mod executable_formats;

#[cfg(test)]
#[path = "../../../src/diagnostics/debug/symbols.rs"]
pub mod debug_symbols;

#[cfg(test)]
pub mod fs {
    pub use crate::filesystem_archive as archive;
    pub use crate::filesystem_primitives as primitives;
}

#[cfg(test)]
pub mod log {
    pub use crate::log_storage as storage;
}

#[cfg(test)]
pub mod time {
    pub use crate::time_queue as queue;
}

#[cfg(test)]
pub mod process {
    pub use crate::executable_formats as formats;
}

#[cfg(test)]
pub mod debug {
    pub use crate::debug_symbols as symbols;
    pub use hyper::debug::*;
}

#[cfg(test)]
pub(crate) use scheduling_authority::authority;

#[cfg(test)]
pub mod diagnostics {
    pub use crate::{debug, log};
}

#[cfg(test)]
pub(crate) use capability_core::kernel::{accounting, object};

// Pure architecture models are included from their production modules above.
#[cfg(test)]
pub(crate) mod arch {
    pub(crate) mod aarch64 {
        pub(crate) use crate::registers;
        pub(crate) mod vm {
            pub(crate) mod vgic {
                pub(crate) use crate::registers;
            }
        }
    }
}
