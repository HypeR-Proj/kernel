// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Exercises checked copies through sparse stage-2 guest memory.

use hyper::mm::PAGE_SIZE;

use crate::kernel::vm::memory::{Error as MemoryError, GuestAddressSpace};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Error {
    Copy(MemoryError),
    DemandZero,
    Identity,
    Payload,
    Statistics,
}

pub(super) fn run() -> Result<(), Error> {
    const BASE: u64 = 0x4000_0000;
    let identifier = crate::kernel::mm::translation_id::reserve::<
        crate::kernel::mm::translation_id::Stage2Vmid,
    >(8)
    .map_err(|_| Error::Identity)?;
    let mut memory =
        GuestAddressSpace::new(identifier, BASE, 2 * PAGE_SIZE).map_err(Error::Copy)?;
    let address = BASE + PAGE_SIZE - 16;

    let mut demand_zero = [0xff; 32];
    memory
        .copy_from(address, &mut demand_zero)
        .map_err(Error::Copy)?;
    if demand_zero != [0; 32] {
        return Err(Error::DemandZero);
    }

    let payload = *b"stage2 checked cross-page bytes";
    memory.copy_to(address, &payload).map_err(Error::Copy)?;
    let mut copied = [0; 31];
    memory
        .copy_from(address, &mut copied)
        .map_err(Error::Copy)?;
    if copied != payload {
        return Err(Error::Payload);
    }
    if memory.statistics().committed_pages != 2 {
        return Err(Error::Statistics);
    }
    if memory.copy_to(BASE + 2 * PAGE_SIZE - 1, &[1, 2]).err() != Some(MemoryError::InvalidRange) {
        return Err(Error::Payload);
    }
    Ok(())
}
