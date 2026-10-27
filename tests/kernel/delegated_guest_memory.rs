// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Delegated words retain the hardware lease and cannot bypass `NativeBlock` ownership.

use super::{Error, GuestMemoryObject, scalar_object};
use crate::accounting::ResourceDomain;
use crate::mm::user_space::VmoObject;

pub(super) fn run(domain: &ResourceDomain) -> Result<(), &'static str> {
    let vmo = VmoObject::try_new_writable(8192, domain).map_err(|_| "delegated backing")?;
    let memory = GuestMemoryObject::try_new(&vmo, domain).map_err(|_| "delegated grant")?;
    if vmo.write(0, &[1]).is_ok() || vmo.try_snapshot(domain).is_ok() {
        return Err("guest grant weakened direct VMO write exclusion");
    }
    let access = memory
        .native_access()
        .map_err(|_| "initial Native access")?;
    if memory.native_access().is_ok() || memory.claim_native_initiator() {
        return Err("overlapping payload accesses or dedication admitted");
    }
    drop(access);
    for (offset, width, value) in [
        (0, 1, 0xa5),
        (4094, 2, 0x1234),
        (4096, 4, 0x12345678),
        (8184, 8, u64::MAX),
    ] {
        scalar_object(&memory, true, offset, width, value).map_err(|_| "scalar store")?;
        if scalar_object(&memory, false, offset, width, 0).map_err(|_| "scalar load")? != value {
            return Err("scalar width mismatch");
        }
    }
    for (offset, width, value) in [
        (1, 2, 0),
        (8192, 1, 0),
        (0, 3, 0),
        (0, 1, 256),
        (u64::MAX - 7, 8, 0),
    ] {
        if scalar_object(&memory, true, offset, width, value) != Err(Error::InvalidArgument) {
            return Err("invalid guest scalar range admitted");
        }
    }
    // Closing the original VMO cannot release backing delegated through a grant.
    drop(vmo);
    if scalar_object(&memory, false, 4094, 2, 0).map_err(|_| "retained grant")? != 0x1234 {
        return Err("source VMO closure lost guest backing");
    }
    if !memory.claim_native_initiator() || memory.claim_native_initiator() {
        return Err("nonlinear native initiator dedication");
    }
    if scalar_object(&memory, false, 0, 1, 0) != Err(Error::Busy)
        || scalar_object(&memory, true, 0, 1, 0) != Err(Error::Busy)
    {
        return Err("dedicated kernel ring admitted Native access");
    }
    Ok(())
}
