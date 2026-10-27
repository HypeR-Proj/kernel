// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Lock-free crash-state publication and debugger-visible storage.
//!
//! This module owns the single-writer slots inspected after a fatal stop. It
//! does not coordinate IPIs, emit diagnostics, or walk stack frames. Writers
//! run with local interrupts masked; release/acquire publication makes complete
//! immutable snapshots visible without taking locks in the crash-stop path.

use core::cell::UnsafeCell;
use core::mem::MaybeUninit;

use hyper::cpu::CpuIndex;
use hyper::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use crate::hal::exception::CrashContext;

pub(super) const MAX_CPUS: usize = hyper::cpu::MAX_CPUS;
const NO_CRASH_OWNER: usize = usize::MAX;
const CRASH_REASON_CAPACITY: usize = 512;

static CRASH_OWNER: AtomicUsize = AtomicUsize::new(NO_CRASH_OWNER);

// Full diagnostics may touch exception stacks, scheduler metadata, symbols,
// interrupt hardware, and runtime vectors only after this publication.
#[unsafe(export_name = "hyper_crash_ready")]
static CRASH_READY: AtomicBool = AtomicBool::new(false);

// A debugger-visible indication that an early context and reason were saved
// without entering code whose prerequisites were not yet available.
#[unsafe(export_name = "hyper_early_crash_stopped")]
static EARLY_CRASH_STOPPED: AtomicBool = AtomicBool::new(false);

static CRASH_IPI_READY: AtomicBool = AtomicBool::new(false);
static STOPPED_CPUS: AtomicUsize = AtomicUsize::new(0);

// Keep debugger-visible crash tables as plain arrays. Their exported layout is
// an inspection ABI, while every runtime writer index is a validated CpuIndex.
#[unsafe(export_name = "hyper_crash_cpu_contexts")]
static CPU_CONTEXTS: [CrashSlot; MAX_CPUS] = [const { CrashSlot::new() }; MAX_CPUS];

#[unsafe(export_name = "hyper_crash_payloads")]
static CRASH_PAYLOADS: [CrashPayloadSlot; MAX_CPUS] = [const { CrashPayloadSlot::new() }; MAX_CPUS];

pub(super) enum OwnerClaim {
    Acquired,
    OwnedByOther,
    Recursive,
}

pub(super) struct CrashPayload {
    context: CrashContext,
    reason: CrashReason,
}

impl CrashPayload {
    pub(super) const fn new(context: CrashContext, reason: CrashReason) -> Self {
        Self { context, reason }
    }

    pub(super) const fn context(&self) -> &CrashContext {
        &self.context
    }

    pub(super) fn reason(&self) -> &str {
        self.reason.as_str()
    }

    pub(super) const fn reason_was_truncated(&self) -> bool {
        self.reason.was_truncated()
    }
}

pub(super) type CrashReason = super::fixed_text::FixedText<CRASH_REASON_CAPACITY>;

pub(super) struct CrashSlot {
    published: AtomicBool,
    context: UnsafeCell<MaybeUninit<CrashContext>>,
}

impl CrashSlot {
    const fn new() -> Self {
        Self {
            published: AtomicBool::new(false),
            context: UnsafeCell::new(MaybeUninit::uninit()),
        }
    }

    fn publish(&self, context: CrashContext) {
        // SAFETY: Each CPU publishes at most once after local exceptions are
        // masked. Release publication makes the complete Copy value visible.
        unsafe { (*self.context.get()).write(context) };
        self.published.store(true, Ordering::Release);
    }

    pub(super) fn read(&self) -> Option<CrashContext> {
        if !self.published.load(Ordering::Acquire) {
            return None;
        }
        // SAFETY: Acquire observed the release after full initialization, and
        // crash contexts are immutable after publication.
        Some(unsafe { *(*self.context.get()).assume_init_ref() })
    }
}

// SAFETY: Per-CPU single-writer publication is synchronized by `published`.
unsafe impl Sync for CrashSlot {}

struct CrashPayloadSlot {
    occupied: AtomicBool,
    payload: UnsafeCell<MaybeUninit<CrashPayload>>,
}

impl CrashPayloadSlot {
    const fn new() -> Self {
        Self {
            occupied: AtomicBool::new(false),
            payload: UnsafeCell::new(MaybeUninit::uninit()),
        }
    }

    fn publish(&self, payload: CrashPayload) -> Option<usize> {
        self.occupied
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()?;
        // SAFETY: The successful transition grants this CPU the only write,
        // and the slot is never reclaimed after fatal handling begins.
        unsafe { (*self.payload.get()).write(payload) };
        Some(self.payload.get() as usize)
    }
}

// SAFETY: `occupied` grants single-writer ownership and initialized payloads
// are immutable for the remainder of fail-stop execution.
unsafe impl Sync for CrashPayloadSlot {}

pub(super) fn publish_payload(cpu: CpuIndex, payload: CrashPayload) -> Option<usize> {
    CRASH_PAYLOADS[cpu.get()].publish(payload)
}

/// Recovers a payload argument returned by [`publish_payload`].
///
/// # Safety
///
/// `argument` must be the unchanged nonzero value returned for the current
/// fatal entry. Published slots are never reclaimed, so the returned snapshot
/// remains immutable for the rest of fail-stop execution.
pub(super) unsafe fn payload_from_argument(argument: usize) -> &'static CrashPayload {
    // SAFETY: The caller supplies the address of an initialized CrashPayload
    // inside the permanent CRASH_PAYLOADS array.
    unsafe { &*(argument as *const CrashPayload) }
}

pub(super) fn publish_context(cpu: CpuIndex, context: CrashContext) {
    CPU_CONTEXTS[cpu.get()].publish(context);
}

pub(super) fn contexts() -> &'static [CrashSlot; MAX_CPUS] {
    &CPU_CONTEXTS
}

#[cfg(feature = "crash-console")]
pub(super) fn context(cpu: usize) -> Option<CrashContext> {
    CPU_CONTEXTS.get(cpu).and_then(CrashSlot::read)
}

pub(super) fn claim_owner(cpu: CpuIndex) -> OwnerClaim {
    match CRASH_OWNER.compare_exchange(
        NO_CRASH_OWNER,
        cpu.get(),
        Ordering::AcqRel,
        Ordering::Acquire,
    ) {
        Ok(_) => OwnerClaim::Acquired,
        Err(owner) if owner != cpu.get() => OwnerClaim::OwnedByOther,
        Err(_) => OwnerClaim::Recursive,
    }
}

pub(super) fn mark_ready() {
    CRASH_READY.store(true, Ordering::Release);
}

pub(super) fn is_ready() -> bool {
    CRASH_READY.load(Ordering::Acquire)
}

pub(super) fn mark_early_stopped() {
    EARLY_CRASH_STOPPED.store(true, Ordering::Release);
}

pub(super) fn mark_ipi_ready() {
    CRASH_IPI_READY.store(true, Ordering::Release);
}

pub(super) fn ipi_ready() -> bool {
    CRASH_IPI_READY.load(Ordering::Acquire)
}

pub(super) fn mark_cpu_stopped() {
    STOPPED_CPUS.fetch_add(1, Ordering::AcqRel);
}

pub(super) fn stopped_cpu_count() -> usize {
    STOPPED_CPUS.load(Ordering::Acquire)
}
