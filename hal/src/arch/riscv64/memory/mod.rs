// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

mod address_space;
mod layout;
mod page_table;

#[cfg(CONFIG_CRASH_CONSOLE)]
pub use address_space::inspect_mapping;
pub(crate) use address_space::prepare;
pub use address_space::{
    ActivationContext, PreparedAddressSpace, SecondaryActivationContext, StackMapping,
};
pub use layout::Riscv64AddressTranslation;
pub use page_table::Error;

pub(crate) use layout::KERNEL_BASE;
