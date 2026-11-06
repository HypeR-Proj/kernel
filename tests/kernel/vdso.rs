// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use crate::accounting::{ResourceDomain, ResourceLimits};
use crate::capability::Rights;
use crate::mm::user_space::{MemoryObjectError, VmoObject};
use crate::object::KernelObject;

pub(super) fn run() -> Result<(), &'static str> {
    let domain =
        ResourceDomain::try_new_root(ResourceLimits::UNLIMITED).map_err(|_| "vDSO domain")?;
    crate::process::vdso::initialize(&domain);
    if !crate::process::vdso::available() {
        return Err("vDSO initialization");
    }
    let writable = VmoObject::try_new_writable(4096, &domain).map_err(|_| "observation backing")?;
    let storage = writable.writable_clone().ok_or("observation storage")?;
    let observation =
        VmoObject::from_observation(storage, &domain).map_err(|_| "observation object")?;
    if observation.supported_rights().bits() & Rights::WRITE.union(Rights::EXECUTE).bits() != 0
        || observation.writable_clone().is_some()
        || observation.snapshot_clone().is_some()
        || observation.executable().is_some()
    {
        return Err("live observation leaked mutable/executable authority");
    }
    if !matches!(
        observation.try_snapshot(&domain),
        Err(MemoryObjectError::WrongVariant)
    ) {
        return Err("live observation permitted a snapshot");
    }
    if !matches!(
        observation.write(0, &[1]),
        Err(MemoryObjectError::WrongVariant)
    ) {
        return Err("live observation permitted writes");
    }
    Ok(())
}
