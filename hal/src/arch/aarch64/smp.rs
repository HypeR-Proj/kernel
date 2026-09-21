// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! `AArch64` CPU identity and PSCI secondary-entry ABI.

use core::arch::asm;
use core::ptr::addr_of;

use hyper::sync::atomic::{AtomicU8, AtomicU64, Ordering};

use super::registers;

const MAX_CPUS: usize = hyper::config::MAX_CPUS as usize;
const UNKNOWN_GIC_AFFINITY: u64 = u64::MAX;

/// Logical-CPU to GIC affinity routing information.
///
/// Firmware CPU identifiers are registered by the boot CPU before the target
/// is started. A Release store publishes each completed entry to any CPU that
/// later sends a targeted SGI after observing scheduler CPU availability.
static CPU_GIC_AFFINITIES: [AtomicU64; MAX_CPUS] =
    [const { AtomicU64::new(UNKNOWN_GIC_AFFINITY) }; MAX_CPUS];
static KERNEL_RPC_REASONS: [AtomicU8; MAX_CPUS] = [const { AtomicU8::new(0) }; MAX_CPUS];

pub fn publish_kernel_rpc(cpu: hyper::cpu::CpuIndex, reasons: u8) -> bool {
    KERNEL_RPC_REASONS[cpu.get()].fetch_or(reasons, Ordering::Release) == 0
}

pub fn take_kernel_rpc() -> u8 {
    KERNEL_RPC_REASONS[super::current_cpu_index()].swap(0, Ordering::Acquire)
}

unsafe extern "C" {
    static aarch64_secondary_entry: u8;
}

/// Parameters consumed by the physical secondary-CPU trampoline.
#[repr(C, align(64))]
pub struct SecondaryBootParameters {
    pub transition_root: u64,
    pub kernel_root: u64,
    pub physical_stack_top: u64,
    pub virtual_stack_top: u64,
    pub cpu_index: u64,
    pub rust_entry: u64,
    pub runtime_vectors: u64,
    pub tcr_el2: u64,
}

impl SecondaryBootParameters {
    pub fn new(
        memory: super::memory::SecondaryActivationContext,
        physical_stack_top: u64,
        virtual_stack_top: u64,
        cpu_index: usize,
        entry: extern "C" fn(usize) -> !,
    ) -> Self {
        Self {
            transition_root: memory.transition_root.get(),
            kernel_root: memory.kernel_root.get(),
            physical_stack_top,
            virtual_stack_top,
            cpu_index: cpu_index as u64,
            rust_entry: entry as *const () as usize as u64,
            runtime_vectors: super::exception::runtime_vector_address(),
            tcr_el2: memory.tcr_el2,
        }
    }
}

/// Converts the linked secondary trampoline into its loaded physical address.
pub fn secondary_entry_physical(
    image_physical_start: u64,
    kernel_virtual_base: u64,
) -> Option<u64> {
    let virtual_address = addr_of!(aarch64_secondary_entry) as u64;
    let offset = virtual_address.checked_sub(kernel_virtual_base)?;
    image_physical_start.checked_add(offset)
}

/// Returns the logical CPU index installed in `TPIDR_EL2` by the entry path.
pub fn current_cpu_index() -> usize {
    let index: u64;
    // SAFETY: TPIDR_EL2 is private to the current EL2 processing element.
    unsafe {
        asm!(
            "mrs {index}, tpidr_el2",
            index = out(reg) index,
            options(nomem, nostack, preserves_flags)
        );
    }
    index as usize
}

/// Returns the normalized MPIDR affinity used by PSCI and CPU topology data.
pub fn current_hardware_id() -> u64 {
    let mpidr: u64;
    // SAFETY: MPIDR_EL1 is read-only at EL2.
    unsafe {
        asm!(
            "mrs {mpidr}, mpidr_el1",
            mpidr = out(reg) mpidr,
            options(nomem, nostack, preserves_flags)
        );
    }
    let affinity_0_to_2 = mpidr & registers::MPIDR_AFF0_TO_2_MASK;
    let affinity_3 = (mpidr >> registers::MPIDR_AFF3_SHIFT) & registers::MPIDR_AFF3_MASK;
    affinity_0_to_2 | (affinity_3 << registers::MPIDR_AFF3_SHIFT)
}

/// Records the boot CPU's logical-to-GIC affinity association.
pub fn initialize_boot_cpu() -> bool {
    let Some(affinity) = hardware_id_to_gic_affinity(current_hardware_id()) else {
        return false;
    };
    if affinity & 0xff >= 16 {
        return false;
    }
    publish_gic_affinity(&CPU_GIC_AFFINITIES[0], affinity)
}

/// Records one firmware-described secondary CPU for targeted SGI routing.
pub fn register_cpu(cpu_index: usize, hardware_id: u64) -> bool {
    let Some(slot) = CPU_GIC_AFFINITIES.get(cpu_index) else {
        return false;
    };
    let Some(affinity) = hardware_id_to_gic_affinity(hardware_id) else {
        return false;
    };
    // Without GIC range-selector support, ICC_SGI1R_EL1 can address only
    // Aff0[3:0]. Reject the CPU before scheduler publication rather than
    // admitting a target which can never receive a prompt reschedule IPI.
    if affinity & 0xff >= 16 {
        return false;
    }
    // The boot CPU is the sole registration owner, so duplicate validation
    // needs no inter-CPU synchronization. The final CAS publishes the route.
    if CPU_GIC_AFFINITIES
        .iter()
        .enumerate()
        .any(|(index, registered)| {
            index != cpu_index && registered.load(Ordering::Relaxed) == u64::from(affinity)
        })
    {
        return false;
    }
    publish_gic_affinity(slot, affinity)
}

/// Returns the GIC affinity published for one logical CPU.
pub fn gic_affinity(cpu_index: usize) -> Option<u32> {
    let affinity = CPU_GIC_AFFINITIES.get(cpu_index)?.load(Ordering::Acquire);
    (affinity != UNKNOWN_GIC_AFFINITY).then_some(affinity as u32)
}

fn hardware_id_to_gic_affinity(hardware_id: u64) -> Option<u32> {
    let affinity_mask = registers::MPIDR_AFF0_TO_2_MASK
        | (registers::MPIDR_AFF3_MASK << registers::MPIDR_AFF3_SHIFT);
    if hardware_id & !affinity_mask != 0 {
        return None;
    }
    let affinity_0_to_2 = hardware_id & registers::MPIDR_AFF0_TO_2_MASK;
    let affinity_3 = (hardware_id >> registers::MPIDR_AFF3_SHIFT) & registers::MPIDR_AFF3_MASK;
    Some((affinity_0_to_2 | (affinity_3 << registers::GIC_AFF3_SHIFT)) as u32)
}

fn publish_gic_affinity(slot: &AtomicU64, affinity: u32) -> bool {
    // A logical CPU route is immutable after publication. Replacing a live
    // affinity could redirect an in-flight or later SGI to a different PE.
    slot.compare_exchange(
        UNKNOWN_GIC_AFFINITY,
        u64::from(affinity),
        Ordering::Release,
        Ordering::Relaxed,
    )
    .is_ok()
}

/// Wakes processing elements waiting in WFE after publishing shared state.
pub fn send_event() {
    // A release store orders memory accesses but does not make the non-memory
    // SEV wait for that store to become visible. Complete prior shared-memory
    // stores before sending the one-shot event, otherwise a waiter can wake,
    // observe stale state, and sleep again after consuming the only event.
    // SAFETY: Shared kernel state is Inner Shareable. DSB ISHST completes only
    // prior stores in that domain; SEV then updates event-register state.
    unsafe { asm!("dsb ishst", "sev", options(nostack, preserves_flags)) };
}

pub const fn prepare_secondary_entry() {}
