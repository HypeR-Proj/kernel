// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! CPU topology, SMP startup, and per-CPU lifecycle.

mod smp;

pub(crate) use hyper::cpu::CpuIndex;
pub use smp::{Capabilities, Error, secondary_entry};
pub(crate) use smp::{
    frozen_topology, online_cpu_count, participating_cpu_count,
    publish_current_online_from_idle_observation,
};

/// Reads and validates the executing CPU's logical kernel index.
///
/// A separate capability token would not enforce a stronger invariant while
/// scheduler threads remain CPU-pinned, so callers receive the domain type
/// directly rather than an ornamental guard.
#[inline]
pub(crate) fn current_index() -> Option<CpuIndex> {
    crate::hal::cpu::current_index()
}

/// Starts secondary CPUs and waits for their local kernel state to come online.
pub(crate) fn initialize() -> Result<(), Error> {
    let (platform, memory, image_physical_start, kernel_base) =
        crate::boot::with_boot_state(|state| {
            (
                state.platform,
                state.memory.secondary_activation_context(),
                state.image_physical_start,
                state.memory.kernel_base(),
            )
        });
    let capabilities = smp::initialize(&platform, memory, image_physical_start, kernel_base)?;
    crate::pr_info!(
        "HypeR: SMP online: {}/{} discovered CPUs",
        capabilities.online_cpus,
        capabilities.discovered_cpus
    );
    Ok(())
}
