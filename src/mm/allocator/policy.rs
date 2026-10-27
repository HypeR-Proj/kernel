// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Kernel ownership and fault policy for the Rust global allocator.

use hyper::hal::interrupt::InterruptMask;
use {
    crate::mm::allocator::heap::CpuLocalCachePolicy,
    crate::mm::allocator::heap::KernelGlobalAllocator,
    crate::mm::allocator::heap::PageAvailability,
};

pub struct KernelAllocatorPolicy;

impl InterruptMask for KernelAllocatorPolicy {
    type State = <crate::hal::irq::LocalMask as InterruptMask>::State;

    fn save_and_disable() -> Self::State {
        crate::hal::irq::LocalMask::save_and_disable()
    }

    fn restore(state: Self::State) {
        crate::hal::irq::LocalMask::restore(state);
    }

    fn wait_for_lock_owner() {
        crate::hal::irq::LocalMask::wait_for_lock_owner();
    }
}

// SAFETY: scheduler::preempt_disable returns a CPU-affine pin before the CPU
// identity is read. Kernel local interrupt masking composes with that pin and
// restores the exact saved state after each bounded slot operation.
unsafe impl CpuLocalCachePolicy for KernelAllocatorPolicy {
    type Pin = crate::task::scheduler::PreemptionGuard;

    fn pin() -> Option<Self::Pin> {
        crate::task::scheduler::preempt_disable().ok()
    }

    fn current_cpu(_pin: &Self::Pin) -> Option<hyper::cpu::CpuIndex> {
        crate::cpu::current_index()
    }

    fn memory_pressure(available: PageAvailability) {
        crate::mm::cache_memory::observe_pressure(available);
    }

    fn memory_released(_available: PageAvailability) {
        crate::fs::cache::worker::page_released();
    }

    fn try_reclaim(pages: usize) -> usize {
        crate::fs::cache::worker::try_reclaim(pages)
    }
}

pub type GlobalAllocator = KernelGlobalAllocator<KernelAllocatorPolicy>;

#[global_allocator]
pub static GLOBAL_ALLOCATOR: GlobalAllocator = GlobalAllocator::new();
