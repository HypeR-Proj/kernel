// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Generation-qualified ownership of the lower-EL return world.
//!
//! A lower-AArch64 vector identifies neither a guest nor a native Process.
//! Guest activation therefore publishes a non-repeating generation alongside
//! the pinned vCPU context identity. Native execution already publishes its
//! own generation in `user_entry`. Each world excludes the other once during
//! run admission, so exception entry consults only the active world's source.

use hyper::sync::atomic::{AtomicPtr, AtomicU64, Ordering};

use super::VcpuContext;

const MAX_CPUS: usize = hyper::config::MAX_CPUS as usize;
const EMPTY: u64 = 0;
const TRANSITION: u64 = u64::MAX;

struct GuestWorldSlot {
    /// A normal generation Release-publishes the preceding context identity.
    generation: AtomicU64,
    context: AtomicPtr<VcpuContext>,
}

impl GuestWorldSlot {
    const fn empty() -> Self {
        Self {
            generation: AtomicU64::new(EMPTY),
            context: AtomicPtr::new(core::ptr::null_mut()),
        }
    }
}

static NEXT_GENERATION: AtomicU64 = AtomicU64::new(1);
static GUEST_WORLDS: [GuestWorldSlot; MAX_CPUS] = [const { GuestWorldSlot::empty() }; MAX_CPUS];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    AlreadyPublished,
    GenerationExhausted,
    InvalidCpu,
    InvalidOwner,
    NativeActive,
    NotPublished,
    TransitionInProgress,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum World {
    Guest { generation: u64 },
    Native { generation: u64 },
    None,
    Corrupt,
}

/// Publishes the pinned guest context which owns the next lower-EL return.
///
/// The caller keeps local interrupts masked and retains the context at this
/// address until [`retire_guest`] succeeds. Ordinary exception dispatch uses
/// the address only as identity. A terminal vector may reconstruct one
/// short-lived exclusive reference after matching the exact generation and
/// pointer, while guest entry retains no Rust reference to the context.
pub(super) fn publish_guest(context: &mut VcpuContext) -> Result<(), Error> {
    if super::user_entry::active_generation().is_some() {
        return Err(Error::NativeActive);
    }
    let cpu = current_cpu()?;
    let generation = next_generation()?;
    let slot = &GUEST_WORLDS[cpu];
    slot.generation
        .compare_exchange(EMPTY, TRANSITION, Ordering::Acquire, Ordering::Relaxed)
        .map_err(|_| Error::AlreadyPublished)?;
    slot.context
        .store(core::ptr::from_mut(context), Ordering::Relaxed);
    slot.generation.store(generation, Ordering::Release);
    Ok(())
}

/// Retires exactly the guest context published by [`publish_guest`].
pub(super) fn retire_guest(context: &mut VcpuContext) -> Result<(), Error> {
    let cpu = current_cpu()?;
    let slot = &GUEST_WORLDS[cpu];
    let generation = slot.generation.load(Ordering::Acquire);
    if generation == EMPTY {
        return Err(Error::NotPublished);
    }
    if generation == TRANSITION {
        return Err(Error::TransitionInProgress);
    }
    let expected = core::ptr::from_mut(context);
    if slot.context.load(Ordering::Relaxed) != expected {
        return Err(Error::InvalidOwner);
    }
    slot.generation
        .compare_exchange(generation, TRANSITION, Ordering::AcqRel, Ordering::Acquire)
        .map_err(|_| Error::InvalidOwner)?;
    slot.context.store(core::ptr::null_mut(), Ordering::Relaxed);
    slot.generation.store(EMPTY, Ordering::Release);
    Ok(())
}

/// Identifies the only world allowed to interpret a lower-AArch64 frame.
pub(super) fn current_world() -> World {
    if let Some(generation) = super::user_entry::active_generation() {
        return World::Native { generation };
    }
    match guest_generation() {
        Ok(None) => World::None,
        Ok(Some(generation)) => World::Guest { generation },
        Err(_) => World::Corrupt,
    }
}

/// Checks the guest slot before a pinned Native run publishes its generation.
pub(super) fn native_world_available() -> bool {
    matches!(guest_generation(), Ok(None))
}

fn guest_generation() -> Result<Option<u64>, Error> {
    let cpu = current_cpu()?;
    let slot = &GUEST_WORLDS[cpu];
    let generation = slot.generation.load(Ordering::Acquire);
    match generation {
        EMPTY => {
            if slot.context.load(Ordering::Relaxed).is_null() {
                Ok(None)
            } else {
                Err(Error::InvalidOwner)
            }
        }
        TRANSITION => Err(Error::TransitionInProgress),
        generation if !slot.context.load(Ordering::Relaxed).is_null() => Ok(Some(generation)),
        _ => Err(Error::InvalidOwner),
    }
}

/// Returns the exact pinned context owned by one observed guest generation.
///
/// The caller is architecture exception entry with local exceptions masked;
/// no ordinary Rust reference to the context may be live across guest entry.
pub(super) fn guest_context(generation: u64) -> Result<core::ptr::NonNull<VcpuContext>, Error> {
    if generation == EMPTY || generation == TRANSITION {
        return Err(Error::InvalidOwner);
    }
    let slot = &GUEST_WORLDS[current_cpu()?];
    if slot.generation.load(Ordering::Acquire) != generation {
        return Err(Error::InvalidOwner);
    }
    core::ptr::NonNull::new(slot.context.load(Ordering::Relaxed)).ok_or(Error::InvalidOwner)
}

/// Closes the exact guest generation after its terminal frame was captured.
pub(super) fn close_captured_guest(
    generation: u64,
    context: core::ptr::NonNull<VcpuContext>,
) -> Result<(), Error> {
    let slot = &GUEST_WORLDS[current_cpu()?];
    if slot.context.load(Ordering::Relaxed) != context.as_ptr() {
        return Err(Error::InvalidOwner);
    }
    slot.generation
        .compare_exchange(generation, TRANSITION, Ordering::AcqRel, Ordering::Acquire)
        .map_err(|_| Error::InvalidOwner)?;
    slot.context.store(core::ptr::null_mut(), Ordering::Relaxed);
    slot.generation.store(EMPTY, Ordering::Release);
    Ok(())
}

fn next_generation() -> Result<u64, Error> {
    let mut current = NEXT_GENERATION.load(Ordering::Relaxed);
    loop {
        if current == EMPTY || current == TRANSITION {
            return Err(Error::GenerationExhausted);
        }
        let next = current
            .checked_add(1)
            .filter(|next| *next != TRANSITION)
            .ok_or(Error::GenerationExhausted)?;
        match NEXT_GENERATION.compare_exchange_weak(
            current,
            next,
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => return Ok(current),
            Err(observed) => current = observed,
        }
    }
}

fn current_cpu() -> Result<usize, Error> {
    let cpu = super::current_cpu_index();
    (cpu < MAX_CPUS).then_some(cpu).ok_or(Error::InvalidCpu)
}
