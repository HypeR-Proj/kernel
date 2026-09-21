// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use hyper::cpu::CpuIndex;
use hyper::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};

const MAX_CPUS: usize = hyper::config::MAX_CPUS as usize;
static APIC_IDS: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(u64::MAX) }; MAX_CPUS];
static ONLINE: [AtomicBool; MAX_CPUS] = [const { AtomicBool::new(false) }; MAX_CPUS];
static RESCHEDULE_ENABLED: [AtomicBool; MAX_CPUS] = [const { AtomicBool::new(false) }; MAX_CPUS];
static KERNEL_RPC_REASONS: [AtomicU8; MAX_CPUS] = [const { AtomicU8::new(0) }; MAX_CPUS];

pub fn publish_kernel_rpc(cpu: CpuIndex, reasons: u8) -> bool {
    KERNEL_RPC_REASONS[cpu.get()].fetch_or(reasons, Ordering::Release) == 0
}

pub fn take_kernel_rpc() -> u8 {
    KERNEL_RPC_REASONS[current_cpu_index()].swap(0, Ordering::Acquire)
}

#[repr(C, align(64))]
pub struct SecondaryBootParameters {
    pub root: u64,
    pub physical_stack_top: u64,
    pub virtual_stack_top: u64,
    pub cpu_index: u64,
    pub rust_entry: u64,
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
            root: memory.root.get(),
            physical_stack_top,
            virtual_stack_top,
            cpu_index: cpu_index as u64,
            rust_entry: entry as *const () as usize as u64,
        }
    }
}

pub fn prepare_secondary_entry() {
    super::exception::install_local_vectors();
}

pub const fn secondary_entry_physical(_image_start: u64, _kernel_base: u64) -> Option<u64> {
    Some(0x8000)
}

pub fn current_cpu_index() -> usize {
    let hardware_id = current_hardware_id();
    APIC_IDS
        .iter()
        .position(|id| id.load(Ordering::Acquire) == hardware_id)
        .unwrap_or(usize::MAX)
}

pub fn current_hardware_id() -> u64 {
    let leaf = core::arch::x86_64::__cpuid_count(0xb, 0);
    u64::from(leaf.edx)
}

pub fn initialize_boot_cpu() -> bool {
    if !publish_apic_id(&APIC_IDS[0], current_hardware_id()) {
        return false;
    }
    ONLINE[0].store(true, Ordering::Release);
    true
}

pub fn register_cpu(cpu_index: usize, hardware_id: u64) -> bool {
    let Some(slot) = APIC_IDS.get(cpu_index) else {
        return false;
    };
    if u32::try_from(hardware_id).is_err()
        || APIC_IDS.iter().enumerate().any(|(index, registered)| {
            index != cpu_index && registered.load(Ordering::Relaxed) == hardware_id
        })
    {
        return false;
    }
    publish_apic_id(slot, hardware_id)
}

fn publish_apic_id(slot: &AtomicU64, hardware_id: u64) -> bool {
    if hardware_id == u64::MAX {
        return false;
    }
    // Boot is the sole route-registration owner. The CAS makes each logical
    // route immutable after publication and rejects accidental re-entry.
    slot.compare_exchange(u64::MAX, hardware_id, Ordering::Release, Ordering::Relaxed)
        .is_ok()
}

pub fn mark_current_cpu_online() {
    if let Some(slot) = ONLINE.get(current_cpu_index()) {
        slot.store(true, Ordering::Release);
        super::tlb::synchronize_online_cpu();
    }
}

/// Visits immutable x2APIC routes for the online CPUs other than `current`.
///
/// Online publication follows route publication. An online slot without a
/// representable route is therefore an architecture invariant violation.
pub(super) fn for_each_online_remote_cpu(current: CpuIndex, mut visit: impl FnMut(CpuIndex, u32)) {
    for index in 0..MAX_CPUS {
        let Some(cpu) = CpuIndex::new(index) else {
            hyper::debug::invariant_failure("x86_64/smp::for_each_online_remote_cpu invariant")
        };
        if cpu == current || !ONLINE[index].load(Ordering::Acquire) {
            continue;
        }
        let hardware_id = APIC_IDS[index].load(Ordering::Acquire);
        let Ok(apic_id) = u32::try_from(hardware_id) else {
            hyper::debug::invariant_failure("x86_64/smp::for_each_online_remote_cpu invariant")
        };
        visit(cpu, apic_id);
    }
}

/// Publishes whether the current CPU has installed the reschedule vector.
pub fn set_reschedule_enabled(enabled: bool) -> bool {
    let index = current_cpu_index();
    let Some(slot) = RESCHEDULE_ENABLED.get(index) else {
        return false;
    };
    slot.store(enabled, Ordering::Release);
    true
}

pub fn send_event() {
    let current = current_cpu_index();
    for cpu in 0..MAX_CPUS {
        if cpu == current {
            continue;
        }
        if let Some(cpu) = CpuIndex::new(cpu) {
            let _ = notify_reschedule(cpu);
        }
    }
}

/// Sends a wake-only reschedule prompt to one online logical CPU.
pub fn notify_reschedule(cpu: CpuIndex) -> bool {
    let index = cpu.get();
    let Some(route) = APIC_IDS.get(index) else {
        return false;
    };
    let Some(online) = ONLINE.get(index) else {
        return false;
    };
    let Some(enabled) = RESCHEDULE_ENABLED.get(index) else {
        return false;
    };
    if !online.load(Ordering::Acquire) || !enabled.load(Ordering::Acquire) {
        return false;
    }
    let hardware_id = route.load(Ordering::Acquire);
    let Ok(apic_id) = u32::try_from(hardware_id) else {
        return false;
    };
    super::interrupt_controller::send_fixed_ipi(
        apic_id,
        hyper::hal::interrupt::InterruptId::new(super::platform::RESCHEDULE_VECTOR),
    )
}

pub fn notify_kernel_rpc(cpu: CpuIndex, reasons: u8) -> bool {
    if !publish_kernel_rpc(cpu, reasons) {
        return true;
    }
    let index = cpu.get();
    let Some(route) = APIC_IDS.get(index) else {
        return false;
    };
    let Some(online) = ONLINE.get(index) else {
        return false;
    };
    if !online.load(Ordering::Acquire) {
        return false;
    }
    let Ok(apic_id) = u32::try_from(route.load(Ordering::Acquire)) else {
        return false;
    };
    super::interrupt_controller::send_fixed_ipi(
        apic_id,
        hyper::hal::interrupt::InterruptId::new(super::platform::KERNEL_RPC_VECTOR),
    )
}
