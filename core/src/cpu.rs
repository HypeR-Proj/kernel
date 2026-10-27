// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Architecture-neutral logical CPU identity and per-CPU storage.
//!
//! This module distinguishes a validated kernel CPU slot from firmware CPU
//! identifiers and raw register values. It does not discover the current CPU
//! or define architecture-specific CPU state.

use core::ops::{Index, IndexMut};

/// Proof that the current execution context cannot migrate to another CPU.
///
/// # Safety
///
/// Implementors must retain non-migratable execution for their entire borrow.
/// The proof does not imply that interrupts are masked or that memory is
/// ordered; consumers use it only for same-PE architectural sequences.
pub unsafe trait PinnedExecution {}

/// Number of logical CPU slots compiled into the kernel.
pub const MAX_CPUS: usize = crate::config::MAX_CPUS as usize;

const _: () = {
    assert!(crate::config::MAX_CPUS > 0);
    assert!(crate::config::MAX_CPUS <= u16::MAX as i64);
};

/// A logical CPU slot validated against [`MAX_CPUS`].
///
/// This value proves only that indexing fixed-capacity kernel storage is safe.
/// It does not prove that the CPU was discovered, is online, or is the CPU
/// currently executing the caller.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[repr(transparent)]
pub struct CpuIndex(usize);

impl CpuIndex {
    pub const BOOT: Self = Self(0);

    /// Validates a raw logical CPU index.
    pub const fn new(index: usize) -> Option<Self> {
        if index < MAX_CPUS {
            Some(Self(index))
        } else {
            None
        }
    }

    pub const fn get(self) -> usize {
        self.0
    }
}

/// Fixed-capacity storage with indexing restricted to validated CPU slots.
///
/// `PerCpu` provides neither synchronization nor execution pinning. Shared
/// users must protect each slot with atomics or an appropriate lock, and code
/// that requires the executing CPU must obtain its index from kernel CPU
/// policy at the point where that requirement is checked.
pub struct PerCpu<T> {
    slots: [T; MAX_CPUS],
}

impl<T> PerCpu<T> {
    pub const fn new(slots: [T; MAX_CPUS]) -> Self {
        Self { slots }
    }

    pub fn iter(&self) -> core::slice::Iter<'_, T> {
        self.slots.iter()
    }
}

impl<T> Index<CpuIndex> for PerCpu<T> {
    type Output = T;

    fn index(&self, index: CpuIndex) -> &Self::Output {
        &self.slots[index.get()]
    }
}

impl<T> IndexMut<CpuIndex> for PerCpu<T> {
    fn index_mut(&mut self, index: CpuIndex) -> &mut Self::Output {
        &mut self.slots[index.get()]
    }
}
