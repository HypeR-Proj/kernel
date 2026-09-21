// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use core::arch::asm;
use core::ptr::addr_of;
use hyper::cpu::CpuIndex;
use hyper::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};

const MAX_CPUS: usize = hyper::config::MAX_CPUS as usize;
static HART_IDS: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(u64::MAX) }; MAX_CPUS];
static HART_ONLINE: [AtomicBool; MAX_CPUS] = [const { AtomicBool::new(false) }; MAX_CPUS];
static KERNEL_RPC_REASONS: [AtomicU8; MAX_CPUS] = [const { AtomicU8::new(0) }; MAX_CPUS];

pub fn publish_kernel_rpc(cpu: CpuIndex, reasons: u8) -> bool {
    KERNEL_RPC_REASONS[cpu.get()].fetch_or(reasons, Ordering::Release) == 0
}

pub fn take_kernel_rpc() -> u8 {
    KERNEL_RPC_REASONS[super::current_cpu_index()].swap(0, Ordering::Acquire)
}

unsafe extern "C" {
    static riscv64_secondary_entry: u8;
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

pub fn secondary_entry_physical(image_start: u64, kernel_base: u64) -> Option<u64> {
    let offset = (addr_of!(riscv64_secondary_entry) as u64).checked_sub(kernel_base)?;
    image_start.checked_add(offset)
}

pub fn current_cpu_index() -> usize {
    let value: usize;
    // SAFETY: TP is reserved by the kernel as the immutable current-CPU index.
    unsafe { asm!("mv {value}, tp", value = out(reg) value, options(nomem, nostack)) };
    value
}

pub fn current_hardware_id() -> u64 {
    HART_IDS
        .get(current_cpu_index())
        .map(|id| id.load(Ordering::Acquire))
        .unwrap_or(u64::MAX)
}

pub fn initialize_boot_hart(hart_id: u64) -> bool {
    // SAFETY: Early boot owns TP before installing the boot-hart CPU index.
    unsafe { asm!("mv tp, zero", options(nomem, nostack)) };
    if !publish_hart_id(&HART_IDS[0], hart_id) {
        return false;
    }
    HART_ONLINE[0].store(true, Ordering::Release);
    true
}

pub fn mark_current_hart_online() {
    if let Some(online) = HART_ONLINE.get(current_cpu_index()) {
        online.store(true, Ordering::Release);
    }
}

pub fn for_each_online_remote_hart(
    mut operation: impl FnMut(u64) -> Result<(), super::sbi::Error>,
) -> Result<(), super::sbi::Error> {
    let current = current_cpu_index();
    for (cpu, online) in HART_ONLINE.iter().enumerate() {
        if cpu == current || !online.load(Ordering::Acquire) {
            continue;
        }
        let hart_id = HART_IDS[cpu].load(Ordering::Acquire);
        if hart_id != u64::MAX {
            operation(hart_id)?;
        }
    }
    Ok(())
}

pub fn register_hart(cpu_index: usize, hart_id: u64) -> bool {
    let Some(slot) = HART_IDS.get(cpu_index) else {
        return false;
    };
    publish_hart_id(slot, hart_id)
}

fn publish_hart_id(slot: &AtomicU64, hart_id: u64) -> bool {
    if hart_id == u64::MAX {
        return false;
    }
    // Boot is the sole route-registration owner. The CAS makes each logical
    // route immutable after publication and rejects accidental re-entry.
    slot.compare_exchange(u64::MAX, hart_id, Ordering::Release, Ordering::Relaxed)
        .is_ok()
}

pub fn send_event() {
    // Release publication orders state before its atomic flag, but does not
    // order that flag before the firmware's later IPI write. Order the flag's
    // memory write before either a memory-backed or I/O-backed interrupt-
    // controller output. FENCE ordering applies to later explicit accesses by
    // this hart across the ECALL privilege transition.
    // SAFETY: FENCE has no pointer operands and is valid in HS mode.
    unsafe { asm!("fence w, ow", options(nostack)) };
    let _ = for_each_online_remote_hart(super::sbi::send_ipi);
}

/// Sends a wake-only reschedule prompt to one registered hart.
///
/// RISC-V retains the request for a cooperative scheduling point until its
/// trap backend gains the same qualified IRQ-tail continuation as `AArch64`.
pub fn notify_reschedule(cpu: CpuIndex) -> bool {
    let Some(hart_id) = HART_IDS.get(cpu.get()).map(|id| id.load(Ordering::Acquire)) else {
        return false;
    };
    if hart_id == u64::MAX {
        return false;
    }
    // Order the pending-state write before either a memory-backed or I/O-backed
    // interrupt-controller output performed by the SBI implementation.
    // SAFETY: FENCE has no pointer operands and is valid in HS mode.
    unsafe { asm!("fence w, ow", options(nostack)) };
    super::sbi::send_ipi(hart_id).is_ok()
}

pub const fn prepare_secondary_entry() {}
