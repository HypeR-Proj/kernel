// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Validated registration of caller-owned virtual-serial ring pages.

use core::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use hyper::abi::native::{
    HYPER_NATIVE_VIRTUAL_SERIAL_OUTPUT_BYTES as BYTES,
    HYPER_NATIVE_VIRTUAL_SERIAL_OUTPUT_CAPACITY as CAPACITY,
    HYPER_NATIVE_VIRTUAL_SERIAL_OUTPUT_HEADER_BYTES as HEADER,
};
use {
    crate::accounting::CommittedCharge, crate::accounting::ResourceAmount,
    crate::accounting::ResourceDomain, crate::accounting::ResourceKind,
};
use {
    crate::mm::user_space::DomainAccount, crate::mm::user_space::ExclusiveHardwareWriteLease,
    crate::mm::user_space::KernelPageBackend, crate::mm::user_space::MemoryObjectError,
    crate::mm::user_space::VmoObject,
};

const PAGES: usize = (BYTES / hyper::mm::PAGE_SIZE) as usize;

/// A registration pins caller-owned VMO pages independently of mappings and
/// handles. Its exclusive write lease excludes direct VMO access, snapshots,
/// and writable userspace aliases. Read-only mappings may coexist.
pub(crate) struct SharedAtomicOutput {
    pages: [usize; PAGES],
    _lease: ExclusiveHardwareWriteLease<KernelPageBackend, DomainAccount>,
    _pinned: CommittedCharge,
    produced: AtomicU64,
    consumed: AtomicU64,
    #[cfg(any(
        not(CONFIG_ARCH_AARCH64),
        all(feature = "kernel-test", feature = "kernel-self-test")
    ))]
    dropped: AtomicU64,
}

impl SharedAtomicOutput {
    pub(crate) fn try_register(
        object: &VmoObject,
        domain: &ResourceDomain,
    ) -> Result<Self, MemoryObjectError> {
        if object.size() != BYTES {
            return Err(MemoryObjectError::Vmo(
                crate::mm::user_space::VmoError::InvalidRange,
            ));
        }
        let pinned = domain
            .reserve(ResourceAmount::ZERO.with(ResourceKind::PinnedPages, PAGES as u64))?
            .commit();
        let storage = object.writable().ok_or(MemoryObjectError::WrongVariant)?;
        let lease = storage.try_exclusive_hardware_write_lease()?;
        storage
            .populate(0, BYTES)
            .map_err(|failure| MemoryObjectError::Vmo(failure.cause))?;
        let mut pages = [0; PAGES];
        for (index, address) in pages.iter_mut().enumerate() {
            let physical = storage.resident_physical_page(index as u64 * hyper::mm::PAGE_SIZE)?;
            *address = crate::mm::memory::linear_address(physical.get())
                .ok_or(MemoryObjectError::WrongVariant)?;
        }
        let output = Self {
            pages,
            _pinned: pinned,
            _lease: lease,
            produced: AtomicU64::new(0),
            consumed: AtomicU64::new(0),
            #[cfg(any(
                not(CONFIG_ARCH_AARCH64),
                all(feature = "kernel-test", feature = "kernel-self-test")
            ))]
            dropped: AtomicU64::new(0),
        };
        output.word(0).store(0, Ordering::Relaxed);
        output.word(8).store(0, Ordering::Relaxed);
        Ok(output)
    }

    fn word(&self, offset: usize) -> &AtomicU64 {
        let page_size = hyper::mm::PAGE_SIZE as usize;
        let address = self.pages[offset / page_size] + offset % page_size;
        // SAFETY: fixed ABI offsets are aligned and inside pinned pages. The
        // exclusive lease excludes other writers and kernel copies/snapshots.
        // Read-only consumers use matching atomic accesses.
        unsafe { &*core::ptr::with_exposed_provenance::<AtomicU64>(address) }
    }

    /// Caller holds the port lock across publication and signal mutation.
    /// Returns true only on the empty-to-nonempty transition.
    #[cfg(any(
        not(CONFIG_ARCH_AARCH64),
        all(feature = "kernel-test", feature = "kernel-self-test")
    ))]
    pub(crate) fn publish(&self, byte: u8) -> bool {
        match self.try_publish(byte) {
            Ok(readable) => readable,
            Err(()) => {
                let dropped = self.dropped.load(Ordering::Relaxed).saturating_add(1);
                self.dropped.store(dropped, Ordering::Relaxed);
                self.word(8).store(dropped, Ordering::Relaxed);
                false
            }
        }
    }

    /// Preserve the caller's byte on pressure. The UART retains it in its TX
    /// FIFO until a consumer acknowledgement makes transport space available.
    pub(crate) fn try_publish(&self, byte: u8) -> Result<bool, ()> {
        let produced = self.produced.load(Ordering::Relaxed);
        let consumed = self.consumed.load(Ordering::Relaxed);
        let Some(slot) = crate::vm::serial::ring::writable_slot(produced, consumed, CAPACITY)
        else {
            return Err(());
        };
        let offset = HEADER as usize + slot;
        let page_size = hyper::mm::PAGE_SIZE as usize;
        let address = self.pages[offset / page_size] + offset % page_size;
        // SAFETY: the offset is bounded by the fixed ring capacity. Output
        // pages stay pinned independently of userspace mappings. Atomic bytes keep a
        // malicious premature consumer acknowledgement from creating a Rust
        // data race with a still-reading user. Correct consumers acknowledge
        // only after reading; the acknowledgement syscall and port lock order reuse.
        unsafe { &*core::ptr::with_exposed_provenance::<AtomicU8>(address) }
            .store(byte, Ordering::Relaxed);
        self.word(0).store(produced + 1, Ordering::Release);
        self.produced.store(produced + 1, Ordering::Relaxed);
        Ok(produced == consumed)
    }

    /// Validates a batch acknowledgement under the same port lock as writers.
    /// A rejected cursor changes neither storage nor readiness.
    pub(crate) fn acknowledge(&self, candidate: u64) -> Result<bool, ()> {
        let produced = self.produced.load(Ordering::Relaxed);
        let consumed = self.consumed.load(Ordering::Relaxed);
        if crate::vm::serial::ring::accept_consumer(produced, consumed, candidate) != candidate {
            return Err(());
        }
        self.consumed.store(candidate, Ordering::Relaxed);
        Ok(candidate != produced)
    }
}
