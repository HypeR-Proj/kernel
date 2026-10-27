// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Bounded Native access to explicitly delegated, hardware-owned guest backing.

use crate::capability::InvocationReceiver;

use super::GuestMemoryObject;
use crate::authority::Rights;
use crate::mm::user_space::UserSlice;
use crate::process::Process;
use crate::vm::service::Error;
use core::sync::atomic::{AtomicU8, AtomicU16, AtomicU32, AtomicU64, Ordering, fence};

pub(crate) const COPY_BYTES: usize = 4096;

fn memory_error(error: crate::mm::user_space::MemoryObjectError) -> Error {
    super::Error::Memory(error).into()
}

/// User copies may fault before/after a device copy, but no user pointer is ever
/// retained or passed to guest memory. A failed write can have partial effects.
#[inline(never)]
pub(crate) fn transfer(
    process: &Process,
    memory: &InvocationReceiver,
    write: bool,
    offset: u64,
    buffer: UserSlice,
) -> Result<(), Error> {
    let rights = if write { Rights::WRITE } else { Rights::READ };
    let memory = memory
        .resolve::<GuestMemoryObject>(rights)
        .map_err(crate::process::ProcessError::from)?;
    let object = memory.object();
    let length = usize::try_from(buffer.length()).map_err(|_| Error::InvalidArgument)?;
    if length == 0 || length > COPY_BYTES {
        return Err(Error::InvalidArgument);
    }
    let backing = object.backing();
    if offset
        .checked_add(length as u64)
        .is_none_or(|end| end > backing.size())
    {
        return Err(Error::InvalidArgument);
    }
    let mut scratch = [0; COPY_BYTES];
    let bytes = &mut scratch[..length];
    if write {
        process.copy_from_user(buffer, bytes)?;
    }
    let first = offset & !4095;
    let end = offset + length as u64;
    for page in (first..end).step_by(4096) {
        backing.populate_page(page).map_err(memory_error)?;
    }
    let access = object.native_access()?;
    if write {
        backing.write_exposed(offset, bytes).map_err(memory_error)?;
    } else {
        backing.read_exposed(offset, bytes).map_err(memory_error)?;
    }
    drop(access);
    if !write {
        process.copy_to_user(buffer, bytes)?;
    }
    Ok(())
}

/// Naturally aligned queue words require one machine access. Byte copies are
/// deliberately separate: they do not promise an atomic snapshot of guest data.
pub(crate) fn scalar(
    memory: &InvocationReceiver,
    write: bool,
    offset: u64,
    width: u32,
    value: u64,
) -> Result<u64, Error> {
    let rights = if write { Rights::WRITE } else { Rights::READ };
    let memory = memory
        .resolve::<GuestMemoryObject>(rights)
        .map_err(crate::process::ProcessError::from)?;
    scalar_object(memory.object(), write, offset, width, value)
}

fn scalar_object(
    object: &GuestMemoryObject,
    write: bool,
    offset: u64,
    width: u32,
    value: u64,
) -> Result<u64, Error> {
    if !matches!(width, 1 | 2 | 4 | 8)
        || !offset.is_multiple_of(u64::from(width))
        || (!write && value != 0)
        || (width < 8 && value >> (width * 8) != 0)
    {
        return Err(Error::InvalidArgument);
    }
    let backing = object.backing();
    if offset
        .checked_add(u64::from(width))
        .is_none_or(|end| end > backing.size())
    {
        return Err(Error::InvalidArgument);
    }
    let page = offset & !4095;
    backing.populate_page(page).map_err(memory_error)?;
    let physical = backing.physical_page(page).map_err(memory_error)?;
    let address = crate::mm::memory::linear_address(physical.get())
        .and_then(|base| base.checked_add((offset - page) as usize))
        .ok_or(Error::InvalidArgument)?;
    let _access = object.native_access()?;
    macro_rules! access {
        ($atomic:ty, $integer:ty) => {{
            // SAFETY: The independent hardware-write lease retains stable,
            // writable backing. The checked naturally aligned word is contained
            // in one page and has no Rust reference to its external payload.
            // Guest accesses follow their hardware protocol, not Rust aliases.
            let word = unsafe { <$atomic>::from_ptr(address as *mut $integer) };
            if write {
                word.store(value as $integer, Ordering::Release);
                0
            } else {
                u64::from(word.load(Ordering::Acquire))
            }
        }};
    }
    let result = match width {
        1 => access!(AtomicU8, u8),
        2 => access!(AtomicU16, u16),
        4 => access!(AtomicU32, u32),
        8 => access!(AtomicU64, u64),
        _ => return Err(Error::InvalidArgument),
    };
    if write {
        // A used-index publication must precede a subsequent available-flags
        // read. Release alone lacks the StoreLoad edge on weakly ordered CPUs.
        fence(Ordering::SeqCst);
    }
    Ok(result)
}

#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
#[path = "../../../tests/kernel/delegated_guest_memory.rs"]
mod test;
#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
pub(crate) fn verify_for_test(
    domain: &crate::accounting::ResourceDomain,
) -> Result<(), &'static str> {
    test::run(domain)
}
