// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

mod address_space;
mod layout;
mod page_table;

#[cfg(CONFIG_CRASH_CONSOLE)]
pub use address_space::inspect_mapping;
pub(crate) use address_space::prepare;
pub use address_space::{
    ActivationContext, Error, PreparedAddressSpace, SecondaryActivationContext, StackMapping,
};
pub use layout::Aarch64AddressTranslation;

pub(super) fn kernel_region_base() -> u64 {
    layout::HOST_LAYOUT.image().base()
}

/// Checks direct-map geometry only; the caller must prove RAM residency and ownership.
pub(super) fn linear_page_address(physical: hyper::mm::PhysicalAddress) -> Option<usize> {
    let value = physical.get();
    if !value.is_multiple_of(hyper::mm::PAGE_SIZE)
        || value.checked_add(hyper::mm::PAGE_SIZE)? > super::address::physical_address_limit()
    {
        return None;
    }
    usize::try_from(
        layout::HOST_LAYOUT
            .linear()
            .alias(value, hyper::mm::PAGE_SIZE)?,
    )
    .ok()
}

/// Returns the permanent bootstrap-stack bounds when `stack_pointer` lies in it.
pub fn bootstrap_stack_bounds(stack_pointer: u64) -> Option<(usize, usize)> {
    let (bottom, top) = layout::HOST_LAYOUT.boot_stack_bounds();
    let bottom = usize::try_from(bottom).ok()?;
    let top = usize::try_from(top).ok()?;
    (bottom as u64 <= stack_pointer && stack_pointer <= top as u64).then_some((bottom, top))
}
